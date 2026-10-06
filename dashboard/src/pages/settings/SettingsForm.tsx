import { useMutation, useQueryClient } from "@tanstack/react-query";
import { Save, Undo2 } from "lucide-react";
import { useMemo, useState } from "react";
import { useForm, useWatch, type FieldErrors, type Resolver } from "react-hook-form";

import { callProblem, saveProblem } from "../../api/access";
import { useApiCall } from "../../api/hooks";
import { CONFIG } from "../../api/management";
import { Alert } from "../../components/Alert";
import { Badge } from "../../components/Badge";
import { Button } from "../../components/Button";
import { Card } from "../../components/Card";
import { CheckboxField } from "../../components/CheckboxField";
import { Code } from "../../components/Code";
import { Dialog } from "../../components/Dialog";
import { ProblemNotice } from "../../components/ProblemNotice";
import { SelectField } from "../../components/SelectField";
import { Spinner } from "../../components/Spinner";
import { Table, Td, Th } from "../../components/Table";
import { TextField } from "../../components/TextField";
import {
  SETTINGS,
  SETTING_IDS,
  STRATEGIES,
  STRATEGY_LABELS,
  describeSetting,
  formValueOf,
  formValuesOf,
  isEdited,
  loadedProblems,
  readEdited,
  settingChanges,
  settingValuesOf,
  type SettingChange,
  type SettingId,
  type SettingValues,
  type SettingsInput,
} from "./settingsModel";

/**
 * Checks the settings edited from the loaded ones (the form's context), and
 * only those: a value in config.yaml the form wouldn't take, left alone,
 * doesn't stop the others being saved. It shows as a warning instead.
 */
const checkEdited: Resolver<SettingsInput, SettingValues, SettingValues> = (input, loaded) => {
  const read = readEdited(input, loaded);
  if (read.values !== null) {
    return { values: read.values, errors: {} };
  }
  const errors: FieldErrors<SettingsInput> = {};
  for (const id of SETTING_IDS) {
    const message = read.problems[id];
    if (message !== undefined) {
      errors[id] = { type: "validate", message };
    }
  }
  return { values: {}, errors };
};

/** A save that stopped at a setting the server refused. */
class SaveStoppedError extends Error {
  constructor(
    readonly saved: SettingChange[],
    readonly failed: SettingChange,
    readonly reason: unknown,
  ) {
    super("save stopped");
    this.name = "SaveStoppedError";
  }
}

function plural(count: number, one: string, many: string): string {
  return `${String(count)} ${count === 1 ? one : many}`;
}

interface ReviewDialogProps {
  changes: SettingChange[] | null;
  pending: boolean;
  error: unknown;
  onSave: () => void;
  onClose: () => void;
}

/** What a save will change, setting by setting, against the server's values now. */
function ReviewDialog({ changes, pending, error, onSave, onClose }: ReviewDialogProps) {
  const moved = changes?.some((change) => change.movedOnServer) === true;
  const stopped = error instanceof SaveStoppedError ? error : null;
  const total = changes?.length ?? 0;
  const notTried = stopped === null ? 0 : total - stopped.saved.length - 1;
  return (
    <Dialog
      open={changes !== null}
      size="lg"
      title="Review the changes"
      onClose={onClose}
      footer={
        error === null ? (
          <>
            <Button onClick={onClose} disabled={pending}>
              Cancel
            </Button>
            <Button variant="primary" data-autofocus onClick={onSave} disabled={pending}>
              {pending ? <Spinner /> : <Save aria-hidden="true" className="size-4" />}
              Save {plural(changes?.length ?? 0, "setting", "settings")}
            </Button>
          </>
        ) : (
          <Button data-autofocus onClick={onClose}>
            Close
          </Button>
        )
      }
    >
      <p className="text-muted">
        Each setting is saved to config.yaml on its own, and the server uses it from then on.
        Nothing else in the file changes.
      </p>
      {moved && (
        <Alert tone="warn" title="Some of these changed on the server since the page loaded">
          <p>
            Someone or something else changed them. &ldquo;Now&rdquo; shows the server&apos;s values
            as they are now; saving replaces them.
          </p>
        </Alert>
      )}
      <Table caption="The settings to save">
        <thead>
          <tr>
            <Th>Setting</Th>
            <Th>Now</Th>
            <Th>After saving</Th>
          </tr>
        </thead>
        <tbody>
          {(changes ?? []).map((change) => (
            <tr key={change.id}>
              <Td>
                <span className="font-medium">{SETTINGS[change.id].label}</span>{" "}
                <Code>{SETTINGS[change.id].configKey}</Code>
                {change.movedOnServer && (
                  <>
                    {" "}
                    <Badge tone="warn">Changed on the server</Badge>
                  </>
                )}
              </Td>
              <Td className="break-all">{describeSetting(change.id, change.now)}</Td>
              <Td className="break-all font-medium">{describeSetting(change.id, change.after)}</Td>
            </tr>
          ))}
        </tbody>
      </Table>
      {error !== null && (
        <>
          {stopped !== null && stopped.saved.length > 0 && (
            <Alert tone="warn" live title="Saved some of the changes">
              <p>
                Saved {stopped.saved.length} of {total}. &ldquo;{SETTINGS[stopped.failed.id].label}
                &rdquo; wasn&apos;t saved
                {notTried > 0 ? `, nor the ${plural(notTried, "setting", "settings")} after it` : ""}.
              </p>
            </Alert>
          )}
          <ProblemNotice problem={saveProblem(stopped?.reason ?? error)} live />
        </>
      )}
    </Dialog>
  );
}

export interface SettingsFormProps {
  /** The server's config, an answer of `GET /config`. */
  config: unknown;
}

/**
 * The settings most often changed, grouped as they are used. A save sends
 * only the settings changed here, each through its own route, after showing
 * what changes against the server's values as they are just then.
 */
export function SettingsForm({ config }: SettingsFormProps) {
  const call = useApiCall();
  const client = useQueryClient();
  const loaded = useMemo(() => settingValuesOf(config), [config]);
  const formValues = useMemo(() => formValuesOf(loaded), [loaded]);
  const form = useForm<SettingsInput, SettingValues, SettingValues>({
    resolver: checkEdited,
    context: loaded,
    mode: "onChange",
    // The server's values, as they come in; a field being edited keeps
    // what the user typed.
    values: formValues,
    resetOptions: { keepDirtyValues: true },
  });
  const current = useWatch({ control: form.control });
  const unsaved = SETTING_IDS.filter((id) => isEdited(current[id], loaded[id]));
  const problems = useMemo(() => loadedProblems(loaded), [loaded]);
  /** The problem with setting `id`'s loaded value, while it is left alone. */
  const warning = (id: SettingId) =>
    problems[id] === undefined || unsaved.includes(id)
      ? undefined
      : `${problems[id]} Saving the other settings leaves it as it is.`;
  const [changes, setChanges] = useState<SettingChange[] | null>(null);
  // The number of settings saved, or "nothing" when the server had them all.
  const [outcome, setOutcome] = useState<number | "nothing" | null>(null);

  /** Marks `id` saved as `value`, so the form no longer counts it unsaved. */
  const settle = (id: SettingId, value: SettingValues[SettingId]) => {
    form.resetField(id, { defaultValue: formValueOf(value) });
  };

  const review = useMutation({
    mutationFn: async ({ edited, base }: { edited: SettingValues; base: SettingValues }) => {
      const freshConfig = await call<unknown>(CONFIG);
      client.setQueryData([CONFIG], freshConfig);
      const fresh = settingValuesOf(freshConfig);
      return { found: settingChanges(base, edited, fresh), edited, fresh };
    },
    onSuccess: ({ found, edited, fresh }) => {
      // An edit the server already has is no longer one.
      for (const id of SETTING_IDS) {
        if (edited[id] === fresh[id]) {
          settle(id, fresh[id]);
        }
      }
      if (found.length === 0) {
        setOutcome("nothing");
        return;
      }
      setChanges(found);
    },
  });

  const save = useMutation({
    mutationFn: async (toSave: SettingChange[]) => {
      const saved: SettingChange[] = [];
      for (const change of toSave) {
        try {
          await call<unknown>(SETTINGS[change.id].path, {
            method: "PATCH",
            json: { value: change.after },
          });
        } catch (reason) {
          throw new SaveStoppedError(saved, change, reason);
        }
        saved.push(change);
      }
      return saved;
    },
    onSuccess: (saved) => {
      for (const change of saved) {
        settle(change.id, change.after);
      }
      setChanges(null);
      setOutcome(saved.length);
    },
    onError: (error) => {
      if (error instanceof SaveStoppedError) {
        for (const change of error.saved) {
          settle(change.id, change.after);
        }
      }
    },
    // Other screens show some of these settings too.
    onSettled: () => client.invalidateQueries(),
  });

  const closeReview = () => {
    setChanges(null);
    save.reset();
  };
  const errors: FieldErrors<SettingsInput> = form.formState.errors;

  return (
    <>
      <form
        noValidate
        className="space-y-4"
        onSubmit={(event) => {
          setOutcome(null);
          review.reset();
          void form.handleSubmit((edited) => {
            review.mutate({ edited, base: loaded });
          })(event);
        }}
      >
        <Card
          title="Proxy"
          description="How open-ferry reaches the providers."
        >
          <TextField
            label="Proxy for outbound requests"
            secret
            revealLabel="Show the proxy address"
            hint={
              <>
                Such as <Code>http://proxy.example:8080</Code>. Enter <Code>direct</Code> to use no
                proxy, or leave it empty to use the <Code>HTTPS_PROXY</Code> and{" "}
                <Code>HTTP_PROXY</Code> environment variables. A credential or key with a proxy of its
                own uses that instead.
              </>
            }
            error={errors.proxyUrl?.message}
            warning={warning("proxyUrl")}
            {...form.register("proxyUrl")}
          />
        </Card>

        <Card
          title="Credentials and retries"
          description="Which credential serves a request, and what happens when one fails."
        >
          <SelectField
            label="How credentials are picked"
            options={STRATEGIES.map((strategy) => ({ value: strategy, label: STRATEGY_LABELS[strategy] }))}
            hint="Round robin takes turns among the credentials that can serve a model. Weighted round robin takes turns in proportion to each credential's weight. Fill first uses the first credential until it reaches a limit, then the next."
            error={errors.routingStrategy?.message}
            {...form.register("routingStrategy")}
          />
          <div className="grid gap-4 sm:grid-cols-3">
            <TextField
              label="Retries"
              inputMode="numeric"
              hint="More rounds of credentials to try after a request fails. 0 tries once."
              error={errors.requestRetry?.message}
              warning={warning("requestRetry")}
              {...form.register("requestRetry")}
            />
            <TextField
              label="Credentials per round"
              inputMode="numeric"
              hint="The most credentials tried in each round. 0 tries all of them."
              error={errors.maxRetryCredentials?.message}
              warning={warning("maxRetryCredentials")}
              {...form.register("maxRetryCredentials")}
            />
            <TextField
              label="Longest wait for a retry (seconds)"
              inputMode="numeric"
              hint="While every credential is resting, how long to wait for one. 0 doesn't wait."
              error={errors.maxRetryInterval?.message}
              warning={warning("maxRetryInterval")}
              {...form.register("maxRetryInterval")}
            />
          </div>
          <CheckboxField
            label="Prefixed credentials need the prefix"
            hint={
              <>
                On, a credential or key with a prefix serves only model names that carry it, such as{" "}
                <Code>team-a/claude-sonnet-4-5</Code>. Off, it serves names without the prefix too.
              </>
            }
            {...form.register("forceModelPrefix")}
          />
        </Card>

        <Card title="Logs and usage" description="What the server records.">
          <CheckboxField
            label="Request logs"
            hint="Saves every request and its response in full, prompts included. Off, only failed requests are saved."
            {...form.register("requestLog")}
          />
          <CheckboxField
            label="Log to files"
            hint="Writes the server's log to main.log in its log directory, where the Logs page reads it, instead of only to its console."
            {...form.register("loggingToFile")}
          />
          <CheckboxField
            label="Debug logging"
            hint="Adds much more detail to the server's log."
            {...form.register("debug")}
          />
          <div className="grid gap-4 sm:grid-cols-2">
            <TextField
              label="Log directory limit (MB)"
              inputMode="numeric"
              hint="Past it, the oldest log files are deleted. 0 means no limit."
              error={errors.logsMaxTotalSizeMb?.message}
              warning={warning("logsMaxTotalSizeMb")}
              {...form.register("logsMaxTotalSizeMb")}
            />
            <TextField
              label="Failed-request logs kept"
              inputMode="numeric"
              hint="The oldest go first. 0 keeps them all."
              error={errors.errorLogsMaxFiles?.message}
              warning={warning("errorLogsMaxFiles")}
              {...form.register("errorLogsMaxFiles")}
            />
          </div>
          <CheckboxField
            label="Usage statistics"
            hint="Records each call's model, tokens and cost for the Usage page. No prompt or response is kept."
            {...form.register("usageStatisticsEnabled")}
          />
        </Card>

        <p className="text-muted">
          The other settings, such as the listener, TLS, management, streaming and the provider
          lists, are in config.yaml: edit it on the config.yaml tab. Provider API keys are on the
          Credentials page.
        </p>

        <div className="sticky bottom-0 z-10 -mx-1 space-y-3 rounded-t-lg border border-line bg-surface px-4 py-3 shadow-lg">
          {typeof outcome === "number" && (
            <Alert tone="ok" live>
              <p>
                Saved {plural(outcome, "setting", "settings")}. The server uses{" "}
                {outcome === 1 ? "it" : "them"} from now on.
              </p>
            </Alert>
          )}
          {outcome === "nothing" && (
            <Alert tone="info" live>
              <p>Nothing to save: the server already has these values.</p>
            </Alert>
          )}
          {review.isError && <ProblemNotice problem={callProblem(review.error)} live />}
          <div className="flex flex-wrap items-center justify-between gap-3">
            <p role="status" className="font-medium">
              {unsaved.length === 0
                ? "No unsaved changes."
                : `${plural(unsaved.length, "unsaved change", "unsaved changes")}.`}
            </p>
            <div className="flex flex-wrap gap-2">
              <Button
                disabled={unsaved.length === 0 || review.isPending}
                onClick={() => {
                  setOutcome(null);
                  review.reset();
                  // reset() takes resetOptions too; here every edit goes.
                  form.reset(formValues, { keepDirtyValues: false });
                }}
              >
                <Undo2 aria-hidden="true" className="size-4" />
                Discard
              </Button>
              <Button type="submit" variant="primary" disabled={unsaved.length === 0 || review.isPending}>
                {review.isPending ? <Spinner /> : <Save aria-hidden="true" className="size-4" />}
                Review and save
              </Button>
            </div>
          </div>
        </div>
      </form>
      <ReviewDialog
        changes={changes}
        pending={save.isPending}
        error={save.error}
        onSave={() => {
          if (changes !== null) {
            save.mutate(changes);
          }
        }}
        onClose={closeReview}
      />
    </>
  );
}
