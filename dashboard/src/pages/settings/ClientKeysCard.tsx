import { zodResolver } from "@hookform/resolvers/zod";
import type { UseQueryResult } from "@tanstack/react-query";
import { Plus, Trash2, Undo2 } from "lucide-react";
import { useId, useRef, useState } from "react";
import { useForm } from "react-hook-form";

import { isUnsupportedRoute } from "../../api/client";
import { Alert } from "../../components/Alert";
import { Badge } from "../../components/Badge";
import { Button } from "../../components/Button";
import { Card } from "../../components/Card";
import { Code } from "../../components/Code";
import { Dialog } from "../../components/Dialog";
import { QueryState } from "../../components/QueryState";
import { SecretText } from "../../components/SecretText";
import { TextField } from "../../components/TextField";
import { z } from "../../lib/zod";
import { generateClientKey, isExampleKey, type ApiKeysAnswer } from "../overview/clientKeys";
import { keyName, keyRows, keysIn, withoutChange, type KeyChange, type KeyRow } from "./keyChanges";

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

interface AddKeyDialogProps {
  open: boolean;
  /** The keys in the list now, saved or not, which the new one may not repeat. */
  listed: readonly string[];
  onClose: () => void;
  onAdd: (key: string) => void;
}

/** Adds a client key to the list, to be saved with the other changes. */
function AddKeyDialog({ open, listed, onClose, onAdd }: AddKeyDialogProps) {
  const formId = useId();
  // A new random key each time the dialog opens (it is mounted afresh);
  // the user may paste another.
  const [initialKey] = useState(generateClientKey);
  const form = useForm<AddForm>({
    resolver: zodResolver(addSchema),
    defaultValues: { key: initialKey },
  });

  return (
    <Dialog
      open={open}
      title="Add a client key"
      onClose={onClose}
      footer={
        <>
          <Button onClick={onClose}>Cancel</Button>
          <Button type="submit" form={formId} variant="primary">
            <Plus aria-hidden="true" className="size-4" />
            Add to the list
          </Button>
        </>
      }
    >
      <form
        id={formId}
        noValidate
        className="space-y-4"
        onSubmit={(event) => {
          void form.handleSubmit(({ key }) => {
            if (listed.includes(key)) {
              form.setError("key", { message: "That key is already in the list." });
              return;
            }
            onAdd(key);
          })(event);
        }}
      >
        <TextField
          label="Client key"
          data-autofocus
          secret
          revealLabel="Show the key"
          hint="A new random key is filled in; paste your own instead if you like. Clients send it as their API key. It works only once you save the changes; until then the list shows it as not saved yet, and you can copy it from there."
          error={form.formState.errors.key?.message}
          {...form.register("key")}
        />
      </form>
    </Dialog>
  );
}

interface KeyItemProps {
  row: KeyRow;
  /** Whether a saved key may be marked for deletion. */
  writable: boolean;
  onDelete: () => void;
  onUndo: () => void;
}

/** One client key: saved, new, or to be deleted, with what can be done to it. */
function KeyItem({ row, writable, onDelete, onUndo }: KeyItemProps) {
  const name = keyName(row.key);
  const saved = row.state === "saved";
  return (
    // The button stays at the end of the first line; a badge wraps under the key.
    <li className="flex items-start justify-between gap-2 py-2">
      <span className="flex min-w-0 flex-wrap items-center gap-2">
        {isExampleKey(row.key) ? (
          <>
            <Code>{row.key}</Code>
            <Badge tone="warn">Example</Badge>
          </>
        ) : (
          <SecretText value={row.key} label={name} />
        )}
        {row.state === "added" && <Badge tone="info">Not saved yet</Badge>}
        {row.state === "deleted" && <Badge tone="danger">Will be deleted</Badge>}
      </span>
      {/* One button that turns from Delete to Undo and back, so focus stays on it. */}
      {(!saved || writable) && (
        <Button size="sm" variant="ghost" className="shrink-0" onClick={saved ? onDelete : onUndo}>
          {saved ? (
            <Trash2 aria-hidden="true" className="size-4" />
          ) : (
            <Undo2 aria-hidden="true" className="size-4" />
          )}
          {saved ? "Delete" : "Undo"}{" "}
          <span className="sr-only">
            {saved ? name : row.state === "added" ? `adding ${name}` : `deleting ${name}`}
          </span>
        </Button>
      )}
    </li>
  );
}

export interface ClientKeysCardProps {
  /** `GET /api-keys`. */
  keys: UseQueryResult<ApiKeysAnswer>;
  /** Keys added and deleted here, not saved yet. */
  changes: readonly KeyChange[];
  onChanges: (changes: KeyChange[]) => void;
  /** Set once a save answers that the server can't save config.yaml. */
  readOnly: boolean;
}

/**
 * The keys clients of the proxy authenticate with: `api-keys` in config.yaml.
 * Adding or deleting one is a change like a setting's edit: it waits for
 * "Review and save".
 */
export function ClientKeysCard({ keys, changes, onChanges, readOnly }: ClientKeysCardProps) {
  const [adding, setAdding] = useState(false);
  // Counts the times the add dialog opened, to mount it afresh each time.
  const [addRound, setAddRound] = useState(0);
  const addButton = useRef<HTMLButtonElement>(null);
  const unsupported = isUnsupportedRoute(keys.error);
  const loaded = keys.data !== undefined;
  const writable = !unsupported && !readOnly && loaded;
  const rows = keyRows(keysIn(keys.data), changes);

  return (
    <Card
      title="Client API keys"
      description="The keys clients send to use the proxy, kept in config.yaml as api-keys."
      actions={
        !writable ? undefined : (
          <Button
            ref={addButton}
            size="sm"
            onClick={() => {
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
      {(unsupported || readOnly) && (
        <Alert tone="info" title="Client keys can't be changed here">
          <p>
            {unsupported
              ? "This server doesn't serve its client keys to the dashboard. "
              : "This server has no way to save config.yaml from here. "}
            Add and delete them under <Code>api-keys</Code> in config.yaml itself, and the server
            picks them up when it reloads the file.
          </p>
        </Alert>
      )}
      {!unsupported &&
        (!loaded ? (
          <QueryState query={keys} loading="Loading the client keys…">
            {() => null}
          </QueryState>
        ) : rows.length === 0 ? (
          <Alert tone="warn" title="No client keys">
            <p>
              The proxy takes requests from anyone who can reach it, with no key at all. Add a key
              to require one.
            </p>
          </Alert>
        ) : (
          <ul className="-my-2 divide-y divide-line">
            {rows.map((row, index) => (
              <KeyItem
                key={`${row.state === "added" ? "new" : "saved"}-${row.key}-${String(index)}`}
                row={row}
                writable={writable}
                onDelete={() => {
                  onChanges([...changes, { kind: "delete", key: row.key }]);
                }}
                onUndo={() => {
                  if (row.state === "added") {
                    onChanges(withoutChange(changes, "add", row.key));
                    // Its row goes, and its button with it.
                    addButton.current?.focus();
                  } else {
                    onChanges(withoutChange(changes, "delete", row.key));
                  }
                }}
              />
            ))}
          </ul>
        ))}
      <AddKeyDialog
        key={addRound}
        open={adding && writable}
        listed={rows.map((row) => row.key)}
        onClose={() => {
          setAdding(false);
        }}
        onAdd={(key) => {
          onChanges([...changes, { kind: "add", key }]);
          setAdding(false);
        }}
      />
    </Card>
  );
}
