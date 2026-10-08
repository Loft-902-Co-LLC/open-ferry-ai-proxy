import type { UseQueryResult } from "@tanstack/react-query";
import { useCallback, useMemo, type ReactNode } from "react";
import { useForm, useWatch, type FieldErrors, type Resolver } from "react-hook-form";

import { Badge } from "../../components/Badge";
import { Card } from "../../components/Card";
import { CheckboxField } from "../../components/CheckboxField";
import { Code } from "../../components/Code";
import { QueryState } from "../../components/QueryState";
import { SelectField } from "../../components/SelectField";
import { TextField } from "../../components/TextField";
import { remoteWarning } from "./managementAddress";
import {
  SETTING_IDS,
  STRATEGIES,
  STRATEGY_LABELS,
  formValueOf,
  formValuesOf,
  isEdited,
  loadedProblems,
  readEdited,
  settingValuesOf,
  type ServerFacts,
  type SettingId,
  type SettingValues,
  type SettingsInput,
} from "./settingsModel";

/** What the form's checks need besides the values typed. */
interface SettingsContext {
  /** The settings as loaded, which the edits are against. */
  loaded: SettingValues;
  /** What the v8 config route said, once it has. */
  facts: ServerFacts | undefined;
}

/**
 * Checks the settings edited from the loaded ones (in the form's context),
 * and only those: a value in config.yaml the form wouldn't take, left alone,
 * doesn't stop the others being saved. It shows as a warning instead.
 */
const checkEdited: Resolver<SettingsInput, SettingsContext, SettingValues> = (input, context) => {
  const read = readEdited(input, context?.loaded, context?.facts);
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

/**
 * The settings form's state, over `config`, an answer of `GET /config`, and
 * `facts`, read through the v8 config route: the values as the server uses
 * them, and which the user has edited. The tab owns it, so one bar and one
 * review cover the settings and client keys.
 */
export function useSettingsForm(config: unknown, facts: ServerFacts | undefined) {
  const loaded = useMemo(() => settingValuesOf(config, facts), [config, facts]);
  const formValues = useMemo(() => formValuesOf(loaded), [loaded]);
  const context = useMemo(() => ({ loaded, facts }), [loaded, facts]);
  const form = useForm<SettingsInput, SettingsContext, SettingValues>({
    resolver: checkEdited,
    context,
    mode: "onChange",
    // The server's values, as they come in; a field being edited keeps
    // what the user typed.
    values: formValues,
    resetOptions: { keepDirtyValues: true },
  });
  const current = useWatch({ control: form.control });
  const unsaved = SETTING_IDS.filter((id) => isEdited(current[id], loaded[id]));
  const problems = useMemo(() => loadedProblems(loaded, facts), [loaded, facts]);

  /** Marks `id` saved as `value`, so the form no longer counts it unsaved. */
  const settle = useCallback(
    (id: SettingId, value: SettingValues[SettingId]) => {
      form.resetField(id, { defaultValue: formValueOf(value) });
    },
    [form],
  );
  /** Drops every edit. */
  const discard = useCallback(() => {
    // reset() takes resetOptions too; here every edit goes.
    form.reset(formValues, { keepDirtyValues: false });
  }, [form, formValues]);

  return { form, loaded, unsaved, problems, settle, discard };
}

export type SettingsFormState = ReturnType<typeof useSettingsForm>;

/** A hint's text, kept to a readable line length on a wide screen. */
function Hint({ children }: { children: ReactNode }) {
  return <span className="block max-w-prose">{children}</span>;
}

export interface SettingsFormProps {
  id: string;
  settings: SettingsFormState;
  /**
   * What the v8 config route says of the management address, or null when
   * the server has no such route: then the field isn't shown.
   */
  facts: UseQueryResult<ServerFacts> | null;
  /** Called when the form is submitted, as by Enter in a field: starts the review. */
  onSubmit: () => void;
}

/**
 * The settings most often changed, grouped as they are used. A save sends
 * only the settings changed here, each through its own route, after showing
 * what changes against the server's values as they are just then.
 */
export function SettingsForm({ id, settings, facts, onSubmit }: SettingsFormProps) {
  const { form, unsaved, problems } = settings;
  const address = useWatch({ control: form.control, name: "managementAddress" });
  /** The problem with setting `id`'s loaded value, while it is left alone. */
  const warning = (setting: SettingId) =>
    problems[setting] === undefined || unsaved.includes(setting)
      ? undefined
      : `${problems[setting]} Saving the other settings leaves it as it is.`;
  const errors: FieldErrors<SettingsInput> = form.formState.errors;

  return (
    <form
      id={id}
      noValidate
      className="space-y-4"
      onSubmit={(event) => {
        event.preventDefault();
        onSubmit();
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

      {facts !== null && (
        <Card title="Management" description="Where the dashboard and the management API are served.">
          <QueryState query={facts} loading="Reading the management address…">
            {({ allowRemote }) => (
              <TextField
                label={
                  <>
                    Management address <Badge className="ml-1">Takes a restart</Badge>
                  </>
                }
                placeholder="127.0.0.1:8318"
                autoComplete="off"
                spellCheck={false}
                autoCapitalize="none"
                hint={
                  <Hint>
                    An address of their own for the dashboard and the management API, apart from
                    the proxy: <Code>127.0.0.1:8318</Code> for this computer only, or{" "}
                    <Code>:8318</Code> for every network interface. Write <Code>host:port</Code>,
                    with an IPv6 address in brackets, such as <Code>[::1]:8318</Code>. The
                    proxy&apos;s port then serves only the proxy. Leave it empty to keep them on the
                    proxy&apos;s port.
                  </Hint>
                }
                error={errors.managementAddress?.message}
                warning={
                  warning("managementAddress") ??
                  (errors.managementAddress === undefined
                    ? (remoteWarning(address, allowRemote) ?? undefined)
                    : undefined)
                }
                {...form.register("managementAddress")}
              />
            )}
          </QueryState>
        </Card>
      )}

      <p className="text-muted">
        The other settings, such as the listener, TLS, the management key, streaming and the
        provider lists, are in config.yaml: edit it on the config.yaml tab. Provider API keys are
        on the Credentials page.
      </p>
    </form>
  );
}
