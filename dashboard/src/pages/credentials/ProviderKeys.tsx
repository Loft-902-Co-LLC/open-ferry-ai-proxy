import { zodResolver } from "@hookform/resolvers/zod";
import { useMutation, useQueryClient, type UseQueryResult } from "@tanstack/react-query";
import { Plus, Trash2 } from "lucide-react";
import { useId, useState } from "react";
import { useForm, useWatch } from "react-hook-form";

import { cantSaveConfig, saveProblem } from "../../api/access";
import { isUnsupportedRoute } from "../../api/client";
import {
  KEY_LISTS,
  KEY_PROVIDERS,
  keysOf,
  writableKey,
  type KeyProvider,
  type ProviderKey,
} from "../../api/credentials";
import { useApiCall, useApiQuery } from "../../api/hooks";
import { Alert } from "../../components/Alert";
import { Button } from "../../components/Button";
import { Card } from "../../components/Card";
import { Code } from "../../components/Code";
import { ConfirmDialog, Dialog } from "../../components/Dialog";
import { ProblemNotice } from "../../components/ProblemNotice";
import { QueryState } from "../../components/QueryState";
import { SecretText } from "../../components/SecretText";
import { SelectField } from "../../components/SelectField";
import { Spinner } from "../../components/Spinner";
import { TextField } from "../../components/TextField";
import { maskKey } from "../../lib/mask";
import { z } from "../../lib/zod";
import { providerKeyAnchor, useAddressAnchor, useFocusAnchor } from "./anchors";
import { providerName } from "./credentialStates";

/** Whether two entries are the same key: the same key and base URL. */
function sameKey(entry: ProviderKey, key: string, baseUrl: string): boolean {
  return entry["api-key"].trim() === key.trim() && (entry["base-url"] ?? "").trim() === baseUrl.trim();
}

/** A key already in a list. */
class DuplicateKeyError extends Error {
  constructor() {
    super("duplicate key");
    this.name = "DuplicateKeyError";
  }
}

/** A key no longer in its list when it came to removing it. */
class KeyGoneError extends Error {
  constructor() {
    super("key gone");
    this.name = "KeyGoneError";
  }
}

function isHttpUrl(value: string): boolean {
  try {
    const url = new URL(value);
    return url.protocol === "http:" || url.protocol === "https:";
  } catch {
    return false;
  }
}

const addSchema = z
  .object({
    provider: z.enum(["claude", "codex", "gemini"]),
    key: z.string().trim().min(1, "Enter the API key."),
    baseUrl: z
      .string()
      .trim()
      .refine((value) => value === "" || isHttpUrl(value), {
        message: "Enter an address starting with http:// or https://, or leave it empty.",
      }),
  })
  .superRefine((value, context) => {
    if (value.provider === "codex" && value.baseUrl === "") {
      context.addIssue({
        code: "custom",
        path: ["baseUrl"],
        message: "A Codex key needs a base URL: the server skips one without it.",
      });
    }
  });

type AddForm = z.infer<typeof addSchema>;

const PROVIDER_OPTIONS = KEY_PROVIDERS.map((provider) => ({
  value: provider,
  label: providerName(provider),
}));

/** Adds a provider API key to its list in config.yaml. */
function AddKeyDialog({
  open,
  providers,
  onClose,
  onAdded,
  onReadOnly,
}: {
  open: boolean;
  /** The lists this server can change. */
  providers: readonly KeyProvider[];
  onClose: () => void;
  onAdded: (provider: KeyProvider) => void;
  /** Called when the server turns out unable to save config.yaml. */
  onReadOnly: () => void;
}) {
  const call = useApiCall();
  const client = useQueryClient();
  const formId = useId();
  const form = useForm<AddForm>({
    resolver: zodResolver(addSchema),
    defaultValues: { provider: providers[0] ?? "claude", key: "", baseUrl: "" },
  });
  const add = useMutation({
    mutationFn: async ({ provider, key, baseUrl }: AddForm) => {
      const { list, path } = KEY_LISTS[provider];
      // The list as it is now, so a change made elsewhere isn't lost.
      const existing = keysOf(await call<unknown>(path), list);
      if (existing.some((entry) => sameKey(entry, key, baseUrl))) {
        throw new DuplicateKeyError();
      }
      const entry: ProviderKey = { "api-key": key.trim() };
      if (baseUrl.trim() !== "") {
        entry["base-url"] = baseUrl.trim();
      }
      await call<unknown>(path, { method: "PUT", json: [...existing.map(writableKey), entry] });
      return provider;
    },
    onSuccess: (provider) => {
      form.reset();
      onAdded(provider);
    },
    onError: (error) => {
      if (cantSaveConfig(error)) {
        onReadOnly();
      }
    },
    onSettled: (_answer, _error, variables) =>
      client.invalidateQueries({ queryKey: [KEY_LISTS[variables.provider].path] }),
  });
  const provider = useWatch({ control: form.control, name: "provider" });
  const errors = form.formState.errors;

  const close = () => {
    form.reset();
    add.reset();
    onClose();
  };

  return (
    <Dialog
      open={open}
      title="Add a provider API key"
      onClose={close}
      footer={
        <>
          <Button onClick={close}>Cancel</Button>
          <Button type="submit" form={formId} variant="primary" disabled={add.isPending}>
            {add.isPending ? <Spinner /> : <Plus aria-hidden="true" className="size-4" />}
            Add the key
          </Button>
        </>
      }
    >
      <form
        id={formId}
        noValidate
        className="space-y-4"
        onSubmit={(event) => {
          void form.handleSubmit((values) => {
            add.mutate(values);
          })(event);
        }}
      >
        <p className="text-muted">
          The server saves it in config.yaml and uses it from the next request on.
        </p>
        <SelectField
          label="Provider"
          options={PROVIDER_OPTIONS.filter((option) => providers.includes(option.value))}
          {...form.register("provider")}
        />
        <TextField
          label="API key"
          data-autofocus
          secret
          revealLabel="Show the key"
          error={errors.key?.message}
          {...form.register("key")}
        />
        <TextField
          label={provider === "codex" ? "Base URL" : "Base URL (optional)"}
          type="url"
          inputMode="url"
          spellCheck={false}
          autoCapitalize="none"
          hint={
            provider === "codex"
              ? "The address of the OpenAI-compatible Responses API to send its requests to."
              : `Leave it empty to use ${providerName(provider)}'s own address.`
          }
          error={errors.baseUrl?.message}
          {...form.register("baseUrl")}
        />
        {add.isError &&
          (add.error instanceof DuplicateKeyError ? (
            <Alert tone="warn" live title="That key is already in the list">
              <p>The server already has this key with this base URL.</p>
            </Alert>
          ) : (
            <ProblemNotice problem={saveProblem(add.error)} live />
          ))}
      </form>
    </Dialog>
  );
}

/** A key's anchor: by the index of the credential it makes, else its place in the list. */
function keyAnchor(provider: KeyProvider, entry: ProviderKey, index: number): string {
  const authIndex = entry["auth-index"]?.trim() ?? "";
  return providerKeyAnchor(provider, authIndex === "" ? String(index) : authIndex);
}

interface KeyEntryProps {
  provider: KeyProvider;
  entry: ProviderKey;
  /** Its element's id, which the address can point at. */
  anchor: string;
  /** Whether the server can save config.yaml, so the key can be deleted. */
  writable: boolean;
  onReadOnly: () => void;
}

/** One key in a list, with a button to delete it. */
function KeyEntry({ provider, entry, anchor, writable, onReadOnly }: KeyEntryProps) {
  const call = useApiCall();
  const client = useQueryClient();
  const name = providerName(provider);
  const [confirm, setConfirm] = useState(false);
  const key = entry["api-key"];
  const baseUrl = entry["base-url"] ?? "";
  const remove = useMutation({
    mutationFn: async () => {
      const { list, path } = KEY_LISTS[provider];
      // Removed by its place in the list as it is now, so the key itself
      // never goes in an address.
      const index = keysOf(await call<unknown>(path), list).findIndex((candidate) =>
        sameKey(candidate, key, baseUrl),
      );
      if (index < 0) {
        throw new KeyGoneError();
      }
      await call<unknown>(path, { method: "DELETE", query: { index } });
    },
    onError: (error) => {
      if (cantSaveConfig(error)) {
        onReadOnly();
      }
    },
    onSettled: () => {
      setConfirm(false);
      return client.invalidateQueries({ queryKey: [KEY_LISTS[provider].path] });
    },
  });

  return (
    <li
      id={anchor}
      tabIndex={-1}
      data-anchor-heading
      className="scroll-mt-4 space-y-2 py-3 first:pt-0 last:pb-0"
    >
      <div className="flex flex-wrap items-center justify-between gap-2">
        <SecretText value={key} label={`the ${name} key ${maskKey(key)}`} />
        {writable && (
          <Button
            size="sm"
            variant="ghost"
            onClick={() => {
              remove.reset();
              setConfirm(true);
            }}
          >
            <Trash2 aria-hidden="true" className="size-4" />
            Delete{" "}
            <span className="sr-only">
              the {name} key {maskKey(key)}
            </span>
          </Button>
        )}
      </div>
      <p className="text-muted">
        {baseUrl === "" ? (
          <>Sent to {name}&apos;s own address.</>
        ) : (
          <>
            Sent to <Code>{baseUrl}</Code>.
          </>
        )}
        {typeof entry.prefix === "string" && entry.prefix !== "" && (
          <>
            {" "}
            Its models are named with the prefix <Code>{entry.prefix}</Code>.
          </>
        )}
      </p>
      {remove.isError &&
        (remove.error instanceof KeyGoneError ? (
          <Alert tone="info" live>
            <p>That key was already deleted.</p>
          </Alert>
        ) : (
          <ProblemNotice problem={saveProblem(remove.error)} live />
        ))}
      <ConfirmDialog
        open={confirm}
        title={`Delete this ${name} key?`}
        confirmLabel="Delete key"
        pending={remove.isPending}
        onConfirm={() => {
          remove.mutate();
        }}
        onCancel={() => {
          setConfirm(false);
        }}
      >
        <p>
          The server stops using <Code>{maskKey(key)}</Code> and deletes it from config.yaml. The
          key stays valid with {name}: revoke it there if it should stop working altogether.
        </p>
      </ConfirmDialog>
    </li>
  );
}

interface KeyListProps {
  provider: KeyProvider;
  query: UseQueryResult;
  writable: boolean;
  onReadOnly: () => void;
}

/** One provider's keys. */
function KeyList({ provider, query, writable, onReadOnly }: KeyListProps) {
  const titleId = useId();
  const name = providerName(provider);
  const address = useAddressAnchor();
  const shown = query.isSuccess ? keysOf(query.data, KEY_LISTS[provider].list) : [];
  const targeted = shown.some((entry, index) => keyAnchor(provider, entry, index) === address);
  useFocusAnchor(targeted ? address : null);
  return (
    <section aria-labelledby={titleId} className="space-y-2">
      <h3 id={titleId} className="font-semibold">
        {name}
      </h3>
      <QueryState query={query} loading={`Loading the ${name} keys…`}>
        {(answer) => {
          const entries = keysOf(answer, KEY_LISTS[provider].list);
          return entries.length === 0 ? (
            <p className="text-muted">None.</p>
          ) : (
            <ul className="divide-y divide-line">
              {entries.map((entry, index) => (
                <KeyEntry
                  key={`${entry["api-key"]}-${entry["base-url"] ?? ""}-${String(index)}`}
                  provider={provider}
                  entry={entry}
                  anchor={keyAnchor(provider, entry, index)}
                  writable={writable}
                  onReadOnly={onReadOnly}
                />
              ))}
            </ul>
          );
        }}
      </QueryState>
    </section>
  );
}

export interface ProviderKeysProps {
  /** Whether the add dialog is open. */
  adding: boolean;
  onAdd: () => void;
  onAddClosed: () => void;
}

/** The provider API keys in config.yaml, which the server sends requests with. */
export function ProviderKeys({ adding, onAdd, onAddClosed }: ProviderKeysProps) {
  const claude = useApiQuery<unknown>(KEY_LISTS.claude.path);
  const codex = useApiQuery<unknown>(KEY_LISTS.codex.path);
  const gemini = useApiQuery<unknown>(KEY_LISTS.gemini.path);
  const queries: Record<KeyProvider, UseQueryResult> = { claude, codex, gemini };
  const [added, setAdded] = useState<KeyProvider | null>(null);
  // Set once a change answers that the server can't save config.yaml.
  const [readOnly, setReadOnly] = useState(false);

  const served = KEY_PROVIDERS.filter((provider) => !isUnsupportedRoute(queries[provider].error));
  const unsupported = served.length === 0;
  const writable = !unsupported && !readOnly;
  const markReadOnly = () => {
    setReadOnly(true);
  };

  return (
    <Card
      title="Provider API keys"
      description="Keys from the providers' consoles, kept in config.yaml. Changes take effect at once."
      actions={
        !writable ? undefined : (
          <Button
            size="sm"
            onClick={() => {
              setAdded(null);
              onAdd();
            }}
          >
            <Plus aria-hidden="true" className="size-4" />
            Add an API key
          </Button>
        )
      }
    >
      {added !== null && (
        <Alert tone="ok" live>
          <p>Added the {providerName(added)} key: the server uses it from now on.</p>
        </Alert>
      )}
      {!writable && (
        <Alert tone="info" title="Provider API keys can't be changed here">
          <p>
            {unsupported
              ? "This server doesn't serve its provider keys to the dashboard. "
              : "This server has no way to save config.yaml from here. "}
            Add and delete them in config.yaml itself, under <Code>claude-api-key</Code>,{" "}
            <Code>codex-api-key</Code> or <Code>gemini-api-key</Code>, and the server picks them up
            when it reloads the file.
          </p>
        </Alert>
      )}
      {served.map((provider) => (
        <KeyList
          key={provider}
          provider={provider}
          query={queries[provider]}
          writable={writable}
          onReadOnly={markReadOnly}
        />
      ))}
      <AddKeyDialog
        open={adding && !unsupported}
        providers={served}
        onClose={onAddClosed}
        onAdded={(provider) => {
          setAdded(provider);
          onAddClosed();
        }}
        onReadOnly={markReadOnly}
      />
    </Card>
  );
}
