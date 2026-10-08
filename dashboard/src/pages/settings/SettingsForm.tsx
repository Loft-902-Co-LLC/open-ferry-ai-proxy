import { useMutation, useQueryClient } from "@tanstack/react-query";
import { Save, Undo2 } from "lucide-react";
import { useMemo, useState, type ReactNode } from "react";
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

/** A hint's text, kept to a readable line length on a wide screen. */
function Hint({ children }: { children: ReactNode }) {
  return <span className="block max-w-prose">{children}</span>;
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
        <Card title="Proxy" description="How the server reaches the providers.">
          <TextField
            label="Proxy for outbound requests"
            secret
            revealLabel="Show the proxy address"
            hint={
              <Hint>
                The proxy the server goes through to reach the providers, for requests, sign-ins and
                token refreshes. Set it when the server can only reach the internet through one: an
                http:// or https:// address, such as <Code>http://proxy.example:8080</Code> (SOCKS
                isn&apos;t supported yet). Enter <Code>direct</Code> for none. Left empty, the server
                uses the <Code>HTTPS_PROXY</Code>, <Code>HTTP_PROXY</Code>, <Code>ALL_PROXY</Code>{" "}
                and <Code>NO_PROXY</Code> environment variables, but not the system&apos;s proxy
                settings. A credential or key with a proxy of its own uses that instead, and{" "}
                <Code>claude-cli</Code> entries use none.
              </Hint>
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
            hint={
              <Hint>
                When more than one credential can serve a model, this picks the one for each request,
                from the ready credentials with the highest <Code>priority</Code>. Round robin takes
                turns. Weighted round robin takes turns in proportion to each credential&apos;s{" "}
                <Code>weight</Code> (1 unless set; 0 leaves it out). Fill first keeps to the first
                credential, in order of their IDs, until it fails and rests or is turned off, then
                moves to the next: choose it to use up one account before the next.
              </Hint>
            }
            error={errors.routingStrategy?.message}
            {...form.register("routingStrategy")}
          />
          <div className="grid gap-4 sm:grid-cols-3">
            <TextField
              label="Retries"
              inputMode="numeric"
              hint={
                <Hint>
                  A request goes once through the credentials, moving on from any that fails. This is
                  how many more times to go through them after a rate limit, a timeout, a server
                  error, a 403 or a lost connection; other errors end the request at once. 0 goes
                  through once. Raise it when requests fail while every credential rests.
                </Hint>
              }
              error={errors.requestRetry?.message}
              warning={warning("requestRetry")}
              {...form.register("requestRetry")}
            />
            <TextField
              label="Credentials per round"
              inputMode="numeric"
              hint={
                <Hint>
                  The most credentials one round tries. 0 tries every one that can serve the model.
                  Lower it for a request to fail sooner.
                </Hint>
              }
              error={errors.maxRetryCredentials?.message}
              warning={warning("maxRetryCredentials")}
              {...form.register("maxRetryCredentials")}
            />
            <TextField
              label="Longest wait for a retry (seconds)"
              inputMode="numeric"
              hint={
                <Hint>
                  Before another round, while every credential rests, the server waits for the first
                  to be ready if that&apos;s within this many seconds; if not, the request fails at
                  once. 0 never waits. It counts only with Retries at 1 or more.
                </Hint>
              }
              error={errors.maxRetryInterval?.message}
              warning={warning("maxRetryInterval")}
              {...form.register("maxRetryInterval")}
            />
          </div>
          <CheckboxField
            label="Prefixed credentials need the prefix"
            hint={
              <Hint>
                A credential or key with a <Code>prefix</Code> serves model names that carry it, such
                as <Code>team-a/claude-sonnet-4-5</Code>. On, that&apos;s all it serves, so it&apos;s
                kept for the requests that ask for it. Off, it serves the plain names too.
              </Hint>
            }
            {...form.register("forceModelPrefix")}
          />
        </Card>

        <Card title="Logs and usage" description="What the server records.">
          <CheckboxField
            label="Request logs"
            hint={
              <Hint>
                Off, the server saves a log file only for each request that fails (
                <Code>error-*.log</Code>). On, it saves one for every request clients send through
                it: what the client sent, and each call to a provider with its answer, prompts and
                replies included, with keys and tokens masked. Turn it on to look into a problem and
                off after: the files hold whole conversations and grow fast. The dashboard&apos;s
                own calls are never logged.
              </Hint>
            }
            {...form.register("requestLog")}
          />
          <CheckboxField
            label="Log to files"
            hint={
              <Hint>
                On, the server writes its log to <Code>main.log</Code> in its log directory instead
                of its console, starting a new file every 10 MB. The Logs page can show the log only
                while this is on.
              </Hint>
            }
            {...form.register("loggingToFile")}
          />
          <CheckboxField
            label="Debug logging"
            hint={
              <Hint>
                Adds the server&apos;s detailed debug messages to its log, for looking into a
                problem. Turn it off after, as the log grows faster with it. The Logs page shows them
                only with Log to files on.
              </Hint>
            }
            {...form.register("debug")}
          />
          <div className="grid gap-4 sm:grid-cols-2">
            <TextField
              label="Log directory limit (MB)"
              inputMode="numeric"
              hint={
                <Hint>
                  Once the log files in the log directory, request logs included, pass this size, the
                  server deletes the oldest, never the <Code>main.log</Code> in use. It checks every
                  minute. 0 means no limit.
                </Hint>
              }
              error={errors.logsMaxTotalSizeMb?.message}
              warning={warning("logsMaxTotalSizeMb")}
              {...form.register("logsMaxTotalSizeMb")}
            />
            <TextField
              label="Failed-request logs kept"
              inputMode="numeric"
              hint={
                <Hint>
                  How many <Code>error-*.log</Code> files to keep, written for failed requests while
                  Request logs is off. The oldest go first. 0 keeps them all; unset, it&apos;s 10.
                </Hint>
              }
              error={errors.errorLogsMaxFiles?.message}
              warning={warning("errorLogsMaxFiles")}
              {...form.register("errorLogsMaxFiles")}
            />
          </div>
          <CheckboxField
            label="Usage statistics"
            hint={
              <Hint>
                Records each call to a provider for the Usage page, retries included: the model, the
                credential, the client key masked, the tokens and the time taken. No prompt or reply
                is kept. Off, nothing new is recorded, and past records stay.
              </Hint>
            }
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
