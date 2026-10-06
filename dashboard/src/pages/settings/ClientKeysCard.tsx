import { zodResolver } from "@hookform/resolvers/zod";
import { useMutation, useQueryClient } from "@tanstack/react-query";
import { Plus, Trash2 } from "lucide-react";
import { useId, useState } from "react";
import { useForm } from "react-hook-form";

import { callProblem, isSettingsReadOnly } from "../../api/access";
import { isUnsupportedRoute } from "../../api/client";
import { useApiCall, useApiQuery } from "../../api/hooks";
import { API_KEYS } from "../../api/management";
import { Alert } from "../../components/Alert";
import { Badge } from "../../components/Badge";
import { Button } from "../../components/Button";
import { Card } from "../../components/Card";
import { Code } from "../../components/Code";
import { ConfirmDialog, Dialog } from "../../components/Dialog";
import { ProblemNotice } from "../../components/ProblemNotice";
import { QueryState } from "../../components/QueryState";
import { SecretText } from "../../components/SecretText";
import { Spinner } from "../../components/Spinner";
import { TextField } from "../../components/TextField";
import { z } from "../../lib/zod";
import {
  generateClientKey,
  isExampleKey,
  maskKey,
  type ApiKeysAnswer,
} from "../overview/clientKeys";

/** A key already in the list. */
class DuplicateKeyError extends Error {
  constructor() {
    super("duplicate key");
    this.name = "DuplicateKeyError";
  }
}

/** A key no longer in the list when it came to removing it. */
class KeyGoneError extends Error {
  constructor() {
    super("key gone");
    this.name = "KeyGoneError";
  }
}

function keysIn(answer: unknown): string[] {
  const list = (answer as Partial<ApiKeysAnswer> | null)?.["api-keys"];
  return Array.isArray(list) ? list.filter((key): key is string => typeof key === "string") : [];
}

const addSchema = z.object({
  key: z
    .string()
    .trim()
    .min(1, "Enter a key.")
    .regex(/^\S+$/, "A key has no spaces in it.")
    .refine((key) => !isExampleKey(key), {
      message:
        "That is one of CLIProxyAPI's examples, which anyone could guess: the proxy goes into safe mode while one is listed.",
    }),
});

type AddForm = z.infer<typeof addSchema>;

/** Adds a client key to `api-keys` in config.yaml. */
function AddKeyDialog({
  open,
  onClose,
  onAdded,
  onReadOnly,
}: {
  open: boolean;
  onClose: () => void;
  onAdded: () => void;
  onReadOnly: () => void;
}) {
  const call = useApiCall();
  const client = useQueryClient();
  const formId = useId();
  // A new random key each time the dialog opens (it is mounted afresh);
  // the user may paste another.
  const [initialKey] = useState(generateClientKey);
  const form = useForm<AddForm>({
    resolver: zodResolver(addSchema),
    defaultValues: { key: initialKey },
  });
  const add = useMutation({
    mutationFn: async ({ key }: AddForm) => {
      // The list as it is now, so a key added elsewhere isn't missed.
      if (keysIn(await call<unknown>(API_KEYS)).includes(key)) {
        throw new DuplicateKeyError();
      }
      // Upstream's patchStringList replaces the first entry equal to `old`
      // and appends `new` when there is none, so this adds the key and
      // leaves the others alone. The key goes in the body, never the URL.
      await call<unknown>(API_KEYS, { method: "PATCH", json: { old: key, new: key } });
    },
    onSuccess: () => {
      onAdded();
    },
    onError: (error) => {
      if (isSettingsReadOnly(error)) {
        onReadOnly();
      }
    },
    onSettled: () => client.invalidateQueries({ queryKey: [API_KEYS] }),
  });

  return (
    <Dialog
      open={open}
      title="Add a client key"
      onClose={onClose}
      footer={
        <>
          <Button onClick={onClose}>Cancel</Button>
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
        <TextField
          label="Client key"
          data-autofocus
          secret
          revealLabel="Show the key"
          hint="A new random key is filled in; paste your own instead if you like. Clients send it as their API key. Copy it from the list once it is added."
          error={form.formState.errors.key?.message}
          {...form.register("key")}
        />
        {add.isError &&
          (add.error instanceof DuplicateKeyError ? (
            <Alert tone="warn" live title="That key is already in the list">
              <p>The server already takes this key.</p>
            </Alert>
          ) : (
            <ProblemNotice
              problem={
                isSettingsReadOnly(add.error) ? { kind: "settings-read-only" } : callProblem(add.error)
              }
              live
            />
          ))}
      </form>
    </Dialog>
  );
}

interface KeyItemProps {
  value: string;
  /** Whether it is the only key, so removing it opens the proxy to anyone. */
  last: boolean;
  writable: boolean;
  onReadOnly: () => void;
}

/** One client key, with a button to remove it. */
function KeyItem({ value, last, writable, onReadOnly }: KeyItemProps) {
  const call = useApiCall();
  const client = useQueryClient();
  const [confirm, setConfirm] = useState(false);
  const example = isExampleKey(value);
  const name = example ? `the example key ${value}` : `the client key ${maskKey(value)}`;
  const remove = useMutation({
    mutationFn: async () => {
      // Removed by its place in the list as it is now, so the key itself
      // never goes in an address.
      const index = keysIn(await call<unknown>(API_KEYS)).indexOf(value);
      if (index < 0) {
        throw new KeyGoneError();
      }
      await call<unknown>(API_KEYS, { method: "DELETE", query: { index } });
    },
    onError: (error) => {
      if (isSettingsReadOnly(error)) {
        onReadOnly();
      }
    },
    onSettled: () => {
      setConfirm(false);
      return client.invalidateQueries({ queryKey: [API_KEYS] });
    },
  });

  return (
    <li className="space-y-2 rounded-md border border-line px-3 py-2">
      <div className="flex flex-wrap items-center justify-between gap-2">
        <span className="inline-flex flex-wrap items-center gap-2">
          {example ? (
            <>
              <Code>{value}</Code>
              <Badge tone="warn">Example</Badge>
            </>
          ) : (
            <SecretText value={value} label={name} />
          )}
        </span>
        {writable && (
          <Button
            size="sm"
            variant="ghost"
            aria-label={`Remove ${name}`}
            onClick={() => {
              remove.reset();
              setConfirm(true);
            }}
          >
            <Trash2 aria-hidden="true" className="size-4" />
            Remove
          </Button>
        )}
      </div>
      {remove.isError &&
        (remove.error instanceof KeyGoneError ? (
          <Alert tone="info" live>
            <p>That key was already removed.</p>
          </Alert>
        ) : (
          <ProblemNotice
            problem={
              isSettingsReadOnly(remove.error)
                ? { kind: "settings-read-only" }
                : callProblem(remove.error)
            }
            live
          />
        ))}
      <ConfirmDialog
        open={confirm}
        title="Remove this client key?"
        confirmLabel="Remove"
        pending={remove.isPending}
        onConfirm={() => {
          remove.mutate();
        }}
        onCancel={() => {
          setConfirm(false);
        }}
      >
        <p>
          Clients that send <Code>{example ? value : maskKey(value)}</Code> are refused from now
          on.
        </p>
        {last && (
          <Alert tone="warn" title="This is the last client key">
            <p>
              With none, the proxy takes requests from anyone who can reach it, with no key at
              all.
            </p>
          </Alert>
        )}
      </ConfirmDialog>
    </li>
  );
}

/** The keys clients of the proxy authenticate with: `api-keys` in config.yaml. */
export function ClientKeysCard() {
  const keys = useApiQuery<ApiKeysAnswer>(API_KEYS);
  const [adding, setAdding] = useState(false);
  // Counts the times the add dialog opened, to mount it afresh each time.
  const [addRound, setAddRound] = useState(0);
  const [added, setAdded] = useState(false);
  // Set once a change answers that the server can't save config.yaml.
  const [readOnly, setReadOnly] = useState(false);
  const unsupported = isUnsupportedRoute(keys.error);
  const writable = !unsupported && !readOnly && keys.isSuccess;
  const markReadOnly = () => {
    setReadOnly(true);
  };

  return (
    <Card
      title="Client API keys"
      description="The keys clients of the proxy send, kept in config.yaml as api-keys. Changes take effect at once."
      actions={
        !writable ? undefined : (
          <Button
            size="sm"
            onClick={() => {
              setAdded(false);
              setAddRound((round) => round + 1);
              setAdding(true);
            }}
          >
            <Plus aria-hidden="true" className="size-4" />
            Add a client key
          </Button>
        )
      }
    >
      {added && (
        <Alert tone="ok" live>
          <p>Added the key: the proxy takes it from now on.</p>
        </Alert>
      )}
      {(unsupported || readOnly) && (
        <Alert tone="info" title="This server can't change settings yet">
          <p>
            {unsupported
              ? "It doesn't serve its client keys to the dashboard. "
              : "It reads config.yaml but has no way to save it. "}
            Add and remove keys under <Code>api-keys</Code> in config.yaml by hand, and the server
            picks them up when it reloads the file.
          </p>
        </Alert>
      )}
      {!unsupported && (
        <QueryState query={keys} loading="Loading the client keys…">
          {(answer) => {
            const list = keysIn(answer);
            if (list.length === 0) {
              return (
                <Alert tone="warn" title="No client keys">
                  <p>
                    The proxy takes requests from anyone who can reach it, with no key at all. Add
                    a key to require one.
                  </p>
                </Alert>
              );
            }
            return (
              <ul className="space-y-2">
                {list.map((value, index) => (
                  <KeyItem
                    key={`${value}-${String(index)}`}
                    value={value}
                    last={list.length === 1}
                    writable={writable}
                    onReadOnly={markReadOnly}
                  />
                ))}
              </ul>
            );
          }}
        </QueryState>
      )}
      <AddKeyDialog
        key={addRound}
        open={adding && !unsupported}
        onClose={() => {
          setAdding(false);
        }}
        onAdded={() => {
          setAdded(true);
          setAdding(false);
        }}
        onReadOnly={markReadOnly}
      />
    </Card>
  );
}
