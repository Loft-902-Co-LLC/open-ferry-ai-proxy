import { useMutation, useQueryClient } from "@tanstack/react-query";
import { ChevronRight, KeyRound, RotateCw } from "lucide-react";
import { useEffect, useId, useRef, useState, type ReactNode, type Ref } from "react";

import { callProblem, saveProblem } from "../../api/access";
import { isUnsupportedRoute } from "../../api/client";
import { CLIENT_SETUP, type ClientSetup } from "../../api/dashboard";
import { useApiCall, useApiQuery } from "../../api/hooks";
import { API_KEYS } from "../../api/management";
import { Alert } from "../../components/Alert";
import { Badge } from "../../components/Badge";
import { Button } from "../../components/Button";
import { Card } from "../../components/Card";
import { Code } from "../../components/Code";
import { CopyButton } from "../../components/CopyButton";
import { ProblemNotice } from "../../components/ProblemNotice";
import { Loading } from "../../components/QueryState";
import { SelectField } from "../../components/SelectField";
import { Spinner } from "../../components/Spinner";
import { Tabs } from "../../components/Tabs";
import { cn } from "../../lib/cn";
import {
  generateClientKey,
  isExampleKey,
  maskKey,
  usableKeys,
  type ApiKeysAnswer,
} from "./clientKeys";
import {
  SHELL_LABELS,
  addressOptions,
  buildSnippets,
  isLoopback,
  type SetupInput,
  type Shell,
  type Snippet,
} from "./snippets";

/** What the setups say while there is no key to put in them. */
const KEY_PLACEHOLDER = "<your client key>";
/** The model choice that leaves each setup its suggestion. */
const SUGGESTED = "";
/** How often to look whether safe mode has lifted, after the keys changed. */
const SAFE_MODE_POLL_MS = 2000;

function defaultShell(): Shell {
  return navigator.userAgent.includes("Windows") ? "powershell" : "posix";
}

/** A key made here, and whether the server has it. */
interface MadeKey {
  key: string;
  saved: boolean;
}

function SafeModeNotice({
  examples,
  replaceLabel,
  pending,
  waiting,
  onReplace,
  actionRef,
}: {
  examples: readonly string[];
  replaceLabel: string;
  pending: boolean;
  waiting: boolean;
  onReplace: () => void;
  actionRef: Ref<HTMLButtonElement>;
}) {
  return (
    <Alert tone="warn" title="The proxy is in safe mode">
      <p>
        Its client keys, <Code>api-keys</Code> in config.yaml, still include CLIProxyAPI&apos;s
        examples
        {examples.length > 0 && (
          <>
            {" "}
            (
            {examples.map((key, index) => (
              <span key={key}>
                {index > 0 && ", "}
                <Code>{key}</Code>
              </span>
            ))}
            )
          </>
        )}
        , which anyone could guess. So it refuses every proxy request, and none of the setups
        below work, until they are replaced.
      </p>
      {waiting ? (
        <p role="status" className="flex items-center gap-2">
          <Spinner /> Saved. Waiting for the proxy to load the new keys and leave safe mode…
        </p>
      ) : (
        <Button ref={actionRef} size="sm" variant="primary" disabled={pending} onClick={onReplace}>
          {pending ? <Spinner /> : <KeyRound aria-hidden="true" className="size-4" />}
          {replaceLabel}
        </Button>
      )}
    </Alert>
  );
}

function SnippetPanel({ shown, copied }: { shown: Snippet; copied: Snippet }) {
  return (
    <div className="space-y-4">
      {shown.modelMissing && (
        <Alert tone="warn">
          <p>
            The model you picked isn&apos;t on <Code>{shown.route.path}</Code> right now, so this
            setup names <Code>{shown.model}</Code>.
          </p>
        </Alert>
      )}
      {shown.parts.map((part, index) => {
        const copyText = copied.parts[index]?.code ?? part.code;
        const step = shown.parts.length > 1 ? `, step ${String(index + 1)}` : "";
        return (
          <div key={part.caption} className="space-y-1.5">
            <div className="flex flex-wrap items-center justify-between gap-2">
              <p className="font-medium">{part.caption}</p>
              <CopyButton text={copyText} label={`Copy the ${shown.label} setup${step}`} />
            </div>
            {/* A region, so its name is read: a bare <pre> has no role to carry one. */}
            <pre
              tabIndex={0}
              role="region"
              aria-label={`The ${shown.label} setup${step}`}
              className="overflow-x-auto rounded-md border border-line bg-raised p-3 font-mono text-xs leading-5"
            >
              {part.code}
            </pre>
          </div>
        );
      })}
      <p className="text-muted">
        As documented at{" "}
        {/* Long addresses break anywhere, so they fit a phone. */}
        <a href={shown.source} target="_blank" rel="noreferrer" className="wrap-anywhere">
          {shown.source}
        </a>
        .
      </p>
    </div>
  );
}

/** A button that shows and hides what it names; its chevron points down while open. */
function DisclosureButton({
  open,
  controls,
  onToggle,
  className,
  children,
}: {
  open: boolean;
  /** The id of what it shows, there only while open. */
  controls: string;
  onToggle: () => void;
  className?: string;
  children: ReactNode;
}) {
  return (
    <button
      type="button"
      aria-expanded={open}
      aria-controls={open ? controls : undefined}
      onClick={onToggle}
      className={cn(
        "inline-flex items-center gap-1.5 rounded-sm pr-1 text-left pointer-coarse:min-h-11",
        className,
      )}
    >
      <ChevronRight
        aria-hidden="true"
        className={cn(
          "size-4 shrink-0 transition-transform motion-reduce:transition-none",
          open && "rotate-90",
        )}
      />
      {children}
    </button>
  );
}

const TITLE = "Connect a client";
const DESCRIPTION =
  "Ready-made setups for common clients, with the proxy's address and a client key filled in.";

export interface ClientSetupCardProps {
  /** Opened from CLIProxyAPI's safe-mode page: bring the key setup into view. */
  focusKeys?: boolean;
}

/**
 * Ready-made setups for clients of the proxy. The main path shows: a
 * client key, a tab per client, and its setup to copy. The address, model
 * and shell are already chosen, and wait behind their own disclosure.
 */
export function ClientSetupCard({ focusKeys = false }: ClientSetupCardProps) {
  const call = useApiCall();
  const client = useQueryClient();
  const [address, setAddress] = useState<string | null>(null);
  const [keyIndex, setKeyIndex] = useState(0);
  const [made, setMade] = useState<MadeKey | null>(null);
  const [model, setModel] = useState<string | null>(null);
  const [shell, setShell] = useState<Shell>(defaultShell);
  const [reveal, setReveal] = useState(false);
  const [tab, setTab] = useState("openai-python");
  const [choicesOpen, setChoicesOpen] = useState(false);
  const choicesId = useId();
  const makeHintId = useId();

  const refresh = () =>
    Promise.all([
      client.invalidateQueries({ queryKey: [API_KEYS] }),
      client.invalidateQueries({ queryKey: [CLIENT_SETUP] }),
    ]);
  // Adds one key and leaves the others alone, so nothing changed meanwhile
  // is overwritten. This is upstream's own behaviour: patchStringList
  // (internal/api/handlers/management/config_lists.go) replaces the first
  // entry equal to `old`, and appends `new` when there is none. `old` is the
  // new key itself, made just now from 32 random bytes, so no entry can
  // equal it. Even if one did, replacing a key with itself changes nothing.
  // The key goes in the body, never the URL.
  const addKey = useMutation({
    mutationFn: (key: string) =>
      call(API_KEYS, { method: "PATCH", json: { old: key, new: key } }),
    onSuccess: async (_, key) => {
      setMade({ key, saved: true });
      await refresh();
    },
  });
  // Safe mode: the examples out of the list and, given `key`, a new key in
  // the first one's place. One entry at a time, against the list as the
  // server has it just then, so a key added meanwhile stays. The new key goes
  // in first, so the list is never left empty, which would let every client
  // in. Then each example goes by its place, the last first so that the
  // places of the others hold. Places keep the keys out of URLs, at the cost
  // of a race: a change to the list from elsewhere meanwhile can move the
  // entries, and then the entry now at a place goes instead. The card reads
  // the list again afterwards.
  const replaceExamples = useMutation({
    mutationFn: async (key: string | null) => {
      const list = [...((await call<ApiKeysAnswer>(API_KEYS))["api-keys"] ?? [])];
      if (key !== null) {
        // As with addKey: the first entry equal to `old` becomes `new`, or
        // `new` is appended when none is.
        const at = list.findIndex(isExampleKey);
        await call(API_KEYS, { method: "PATCH", json: { old: list[at] ?? key, new: key } });
        if (at < 0) {
          list.push(key);
        } else {
          list[at] = key;
        }
      }
      for (let index = list.length - 1; index >= 0; index--) {
        if (isExampleKey(list[index] ?? "")) {
          await call(API_KEYS, { method: "DELETE", query: { index: String(index) } });
        }
      }
    },
    onSettled: refresh,
  });
  // The server saves config.yaml and loads it again before it answers, so it
  // has normally left safe mode by the time the keys are saved. Should it
  // still be in it, look again until it isn't.
  const setup = useApiQuery<ClientSetup>(CLIENT_SETUP, undefined, {
    refetchInterval: (query) =>
      replaceExamples.isSuccess && query.state.data?.safe_mode === true ? SAFE_MODE_POLL_MS : false,
  });
  const keys = useApiQuery<ApiKeysAnswer>(API_KEYS);

  const card = useRef<HTMLDivElement>(null);
  const safeModeAction = useRef<HTMLButtonElement>(null);
  const keySelect = useRef<HTMLSelectElement>(null);

  const saved = keys.data?.["api-keys"] ?? [];
  const examples = saved.filter(isExampleKey);
  const usable = usableKeys(saved);
  const choices = made !== null && !usable.includes(made.key) ? [...usable, made.key] : usable;
  const chosenKey = choices[Math.min(keyIndex, choices.length - 1)] ?? null;

  const makeKey = () => {
    const key = generateClientKey();
    setMade({ key, saved: false });
    setKeyIndex(usable.length);
    addKey.mutate(key);
  };
  const replace = () => {
    if (usable.length > 0) {
      replaceExamples.mutate(null);
      return;
    }
    const key = made?.key ?? generateClientKey();
    setMade({ key, saved: false });
    setKeyIndex(0);
    replaceExamples.mutate(key);
  };

  const loaded = setup.data !== undefined;
  const safeMode = setup.data?.safe_mode === true;
  // Opened from the safe-mode page: once the setup shows, take the user to it.
  const focused = useRef(false);
  useEffect(() => {
    if (!focusKeys || !loaded || focused.current) {
      return;
    }
    focused.current = true;
    card.current?.scrollIntoView({ block: "start" });
    (safeMode ? safeModeAction.current : keySelect.current)?.focus();
  }, [focusKeys, loaded, safeMode]);

  const frame = (content: ReactNode) => (
    <div ref={card} className="scroll-mt-4">
      <Card title={TITLE} description={DESCRIPTION}>
        {content}
      </Card>
    </div>
  );

  if (setup.isPending) {
    return frame(<Loading>Reading the proxy&apos;s setup…</Loading>);
  }
  if (setup.isError) {
    return frame(
      <ProblemNotice
        problem={callProblem(setup.error)}
        action={
          <Button
            size="sm"
            onClick={() => {
              void setup.refetch();
            }}
          >
            <RotateCw aria-hidden="true" className="size-4" />
            Try again
          </Button>
        }
      />,
    );
  }

  const setupData = setup.data;
  const addresses = addressOptions(window.location.origin, setupData.base_urls);
  const root = addresses.find((option) => option.root === address)?.root ?? addresses[0]?.root ?? "";
  const models = setupData.models;
  // A model picked that the server no longer describes gives way to the
  // suggestions.
  const picked = model !== null && models.some((info) => info.id === model) ? model : null;

  const input: Omit<SetupInput, "key"> = {
    root,
    model: picked,
    models,
    routes: setupData.routes,
    shell,
  };
  const shownKey = chosenKey === null ? KEY_PLACEHOLDER : reveal ? chosenKey : maskKey(chosenKey);
  const shownSnippets = buildSnippets({ ...input, key: shownKey });
  const copiedSnippets = buildSnippets({ ...input, key: chosenKey ?? KEY_PLACEHOLDER });
  const selected = shownSnippets.find((snippet) => snippet.id === tab) ?? shownSnippets[0];
  const copied = copiedSnippets.find((snippet) => snippet.id === selected?.id);

  const keysUnsupported = keys.isError && isUnsupportedRoute(keys.error);
  const writeError = addKey.error ?? replaceExamples.error;
  const keyOptions =
    choices.length === 0
      ? [{ value: "0", label: keys.isPending ? "Loading…" : "No client keys yet" }]
      : choices.map((key, index) => ({
          value: String(index),
          label:
            made?.key === key && !made.saved && !usable.includes(key)
              ? `${maskKey(key)} (made here, not saved)`
              : maskKey(key),
        }));

  return frame(
    <>
      {safeMode && (
        <SafeModeNotice
          examples={examples}
          replaceLabel={
            usable.length > 0 ? "Delete the example keys" : "Replace the example keys with a new key"
          }
          pending={replaceExamples.isPending}
          waiting={replaceExamples.isSuccess}
          onReplace={replace}
          actionRef={safeModeAction}
        />
      )}
      {replaceExamples.isSuccess && !safeMode && (
        <Alert tone="ok" live title="The proxy is out of safe mode">
          <p>
            {replaceExamples.variables === null
              ? "The example keys are gone from config.yaml, so it serves proxy requests again."
              : "A new key took the example keys' place in config.yaml, so it serves proxy requests again. The setups below use the new key."}
          </p>
        </Alert>
      )}
      {focusKeys && !safeMode && !replaceExamples.isSuccess && (
        <Alert tone="ok" title="The proxy isn't in safe mode">
          <p>Its client keys are no longer the examples, so it serves proxy requests.</p>
        </Alert>
      )}
      {writeError !== null && <ProblemNotice problem={saveProblem(writeError)} live />}
      {keysUnsupported && (
        <Alert tone="info" title="This server doesn't list its client keys">
          <p>
            The setups below show where the key goes. Make one here and add it to{" "}
            <Code>api-keys</Code> in config.yaml, or use one already there.
          </p>
        </Alert>
      )}
      {keys.isError && !keysUnsupported && <ProblemNotice problem={callProblem(keys.error)} />}
      {addKey.isSuccess && made?.saved === true && (
        <Alert tone="ok" live>
          <p>
            Added a client key. It&apos;s in config.yaml now, and the setups below use it; show it
            or copy it from them at any time.
          </p>
        </Alert>
      )}
      {models.length === 0 && (
        <Alert tone="info" title="No models yet">
          <p>
            The proxy has no credentials that can serve a model. Add a provider&apos;s API key to
            config.yaml, or sign in to Claude or Codex, and its models show here.
          </p>
        </Alert>
      )}

      <div className="space-y-1.5">
        <SelectField
          ref={keySelect}
          label="Client key"
          className="max-w-md"
          value={String(Math.min(keyIndex, Math.max(choices.length - 1, 0)))}
          options={keyOptions}
          disabled={choices.length === 0}
          onChange={(event) => {
            setKeyIndex(Number(event.target.value));
          }}
        />
        <div className="flex flex-wrap items-center gap-x-3 gap-y-1.5">
          <Button
            size="sm"
            disabled={addKey.isPending}
            aria-describedby={makeHintId}
            onClick={makeKey}
          >
            {addKey.isPending ? <Spinner /> : <KeyRound aria-hidden="true" className="size-4" />}
            Make a new key
          </Button>
          <p id={makeHintId} className="text-muted">
            A new key is saved to config.yaml as soon as it&apos;s made.
          </p>
        </div>
      </div>
      <label className="flex items-start gap-2">
        <input
          type="checkbox"
          checked={reveal}
          disabled={chosenKey === null}
          onChange={(event) => {
            setReveal(event.target.checked);
          }}
          className="mt-1 size-4 shrink-0 accent-accent"
        />
        <span>
          Show the key in the setups{" "}
          <span className="text-muted">(Copy always copies it whole.)</span>
        </span>
      </label>

      {selected === undefined || copied === undefined ? (
        <p className="text-muted">The proxy lists none of the routes these setups call.</p>
      ) : (
        <Tabs
          label="Client setups"
          items={shownSnippets.map((snippet) => ({ id: snippet.id, label: snippet.label }))}
          selected={selected.id}
          onSelect={setTab}
        >
          <SnippetPanel shown={selected} copied={copied} />
        </Tabs>
      )}
      {chosenKey === null && !keys.isPending && (
        <p className="flex items-center gap-2 text-muted">
          <Badge tone="warn">No key</Badge> The setups show where a client key goes. Make one
          above.
        </p>
      )}

      <div className="space-y-4 border-t border-line pt-4">
        <DisclosureButton
          open={choicesOpen}
          controls={choicesId}
          onToggle={() => {
            setChoicesOpen(!choicesOpen);
          }}
          className="font-medium"
        >
          Address, model and shell
        </DisclosureButton>
        {choicesOpen && (
          <div id={choicesId} className="grid gap-4 sm:grid-cols-2">
            <SelectField
              label="Address"
              value={root}
              options={addresses.map((option) => ({ value: option.root, label: option.label }))}
              onChange={(event) => {
                setAddress(event.target.value);
              }}
              hint={
                isLoopback(root)
                  ? "This address works only on the computer the proxy runs on."
                  : undefined
              }
            />
            <SelectField
              label="Model"
              value={picked ?? SUGGESTED}
              disabled={models.length === 0}
              options={
                models.length === 0
                  ? [{ value: SUGGESTED, label: "No models yet" }]
                  : [
                      { value: SUGGESTED, label: "Suggested for each setup" },
                      ...models.map((info) => ({
                        value: info.id,
                        label:
                          info.display_name === null || info.display_name === info.id
                            ? info.id
                            : `${info.display_name} (${info.id})`,
                      })),
                    ]
              }
              onChange={(event) => {
                setModel(event.target.value === SUGGESTED ? null : event.target.value);
              }}
              hint={
                picked === null && models.length > 0
                  ? "Each setup names the newest chat model the proxy serves. Claude Code gets the newest Claude model and Codex CLI the newest OpenAI model, if the proxy has one."
                  : undefined
              }
            />
            <SelectField
              label="Shell"
              value={shell}
              options={(Object.keys(SHELL_LABELS) as Shell[]).map((value) => ({
                value,
                label: SHELL_LABELS[value],
              }))}
              onChange={(event) => {
                setShell(event.target.value as Shell);
              }}
              hint="For the setups run in a terminal."
            />
          </div>
        )}
      </div>
    </>,
  );
}
