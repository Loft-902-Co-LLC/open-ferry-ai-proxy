import { yamlLanguage } from "@codemirror/lang-yaml";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Eye, FileDiff, RotateCw, Save, Undo2 } from "lucide-react";
import { Suspense, lazy, useEffect, useId, useState } from "react";

import { callProblem, saveProblem } from "../../api/access";
import { isApiError } from "../../api/client";
import { useConfigEditor } from "../../api/hooks";
import { ConfigEditor, RecoveryEnded, StoredConfigUnusable, type ConfigRead } from "../../api/configEditor";
import { CONFIG_YAML } from "../../api/management";
import { Alert } from "../../components/Alert";
import { Button } from "../../components/Button";
import { Card } from "../../components/Card";
import { Code } from "../../components/Code";
import { ConfirmDialog, Dialog } from "../../components/Dialog";
import { ProblemNotice } from "../../components/ProblemNotice";
import { Loading } from "../../components/QueryState";
import { Spinner } from "../../components/Spinner";
import { cn } from "../../lib/cn";
import { diffCounts, diffLines, diffRows, type DiffLine } from "../../lib/lineDiff";

const YamlEditor = lazy(() =>
  import("./YamlEditor").then((module) => ({ default: module.YamlEditor })),
);

/** An answer of `GET /config.yaml` that isn't the file's text. */
class NotTextError extends Error {
  constructor() {
    super("config.yaml didn't come back as text");
    this.name = "NotTextError";
  }
}

function plural(count: number, one: string, many: string): string {
  return `${count.toLocaleString("en")} ${count === 1 ? one : many}`;
}

/** The highlighting parser is permissive, so only name a line it marks. */
function yamlErrorLine(text: string): number | null {
  const cursor = yamlLanguage.parser.parse(text).cursor();
  do {
    if (cursor.type.isError) return text.slice(0, cursor.from).split(/\r\n?|\n/).length;
  } while (cursor.next());
  return null;
}

/** Why the server didn't save the file, in its words where it gave them. */
function SaveProblem({ error, draft, onReload }: { error: unknown; draft: string; onReload: () => void }) {
  if (isApiError(error) && (error.code === "invalid_yaml" || (error.status === 400 && error.code === "invalid_body"))) {
    const line = yamlErrorLine(draft);
    return (
      <Alert tone="danger" live title="That isn't valid YAML">
        <p>{line !== null && `Check line ${String(line)}. `}{error.detail} Nothing was saved.</p>
      </Alert>
    );
  }
  if (error instanceof RecoveryEnded || error instanceof StoredConfigUnusable) {
    return <Alert tone="warn" live title={error instanceof RecoveryEnded ? "config.yaml can be read again" : "The stored config.yaml has become unusable"}>
      <p>{error.message}</p>
      {error instanceof StoredConfigUnusable && error.detail !== null && <p>{error.detail}</p>}
      <Button onClick={onReload}>Reload latest config</Button>
    </Alert>;
  }
  if (isApiError(error) && error.code === "invalid_config") {
    return (
      <Alert tone="danger" live title="The server can't use this config">
        <p>{error.detail ?? "It didn't say why."} Nothing was saved.</p>
      </Alert>
    );
  }
  return (
    <ProblemNotice
      problem={saveProblem(error)}
      onReloadConfig={onReload}
      live
    />
  );
}

function lineClasses(line: DiffLine): string {
  switch (line.kind) {
    case "added":
      return "bg-ok-soft";
    case "removed":
      return "bg-danger-soft";
    default:
      return "";
  }
}

/** The lines a save changes, with three lines around each change. */
function DiffView({ lines }: { lines: readonly DiffLine[] }) {
  const rows = diffRows(lines);
  const { added, removed } = diffCounts(lines);
  return (
    <div
      role="region"
      aria-label="The changes to config.yaml"
      tabIndex={0}
      className="max-h-[50vh] overflow-auto rounded-md border border-line"
    >
      <table className="w-full border-collapse font-mono text-xs leading-5">
        <caption className="sr-only">
          {plural(added, "line", "lines")} added and {plural(removed, "line", "lines")} removed.
          Each line shows its number before and after the change.
        </caption>
        <tbody>
          {rows.map((row, index) => {
            if (row.kind === "gap") {
              return (
                <tr key={`gap-${String(index)}`} className="bg-raised text-muted">
                  <td colSpan={4} className="px-2 py-0.5 font-sans">
                    {plural(row.count, "unchanged line", "unchanged lines")}
                  </td>
                </tr>
              );
            }
            const { line } = row;
            const marker = line.kind === "added" ? "+" : line.kind === "removed" ? "-" : " ";
            const text = line.text === "" ? " " : line.text;
            return (
              <tr key={`${String(line.before)}-${String(line.after)}`} className={lineClasses(line)}>
                <td className="w-px px-2 text-right whitespace-nowrap text-muted select-none">
                  {line.before ?? ""}
                </td>
                <td className="w-px px-2 text-right whitespace-nowrap text-muted select-none">
                  {line.after ?? ""}
                </td>
                <td
                  aria-hidden="true"
                  className={cn(
                    "w-px px-1 font-semibold select-none",
                    line.kind === "added" && "text-ok",
                    line.kind === "removed" && "text-danger",
                  )}
                >
                  {marker}
                </td>
                <td className="pr-2 whitespace-pre">
                  {line.kind === "added" ? (
                    <ins className="no-underline">
                      <span className="sr-only">Added: </span>
                      {text}
                    </ins>
                  ) : line.kind === "removed" ? (
                    <del className="no-underline">
                      <span className="sr-only">Removed: </span>
                      {text}
                    </del>
                  ) : (
                    text
                  )}
                </td>
              </tr>
            );
          })}
        </tbody>
      </table>
    </div>
  );
}

interface Review {
  /** The file on the server just now. */
  fresh: ConfigRead<string>;
  /** What saving writes. */
  draft: string;
}

interface ReviewDialogProps {
  review: Review | null;
  /** The file as it was when the editor opened it. */
  base: ConfigRead<string>;
  pending: boolean;
  error: unknown;
  onSave: () => void;
  onClose: () => void;
  onReload: () => void;
}

/** The diff a save makes against the file as the server has it now. */
function ReviewDialog({ review, base, pending, error, onSave, onClose, onReload }: ReviewDialogProps) {
  const lines = review === null ? [] : diffLines(review.fresh.data, review.draft);
  const { added, removed } = diffCounts(lines);
  const same = review !== null && review.fresh.data === review.draft;
  const moved = review !== null && (base.etag !== undefined ? review.fresh.etag !== base.etag : review.fresh.data !== base.data);
  return (
    <Dialog
      open={review !== null}
      size="lg"
      title="Review the changes to config.yaml"
      onClose={onClose}
      footer={
        same || error !== null ? (
          <Button data-autofocus onClick={onClose}>
            Close
          </Button>
        ) : (
          <>
            <Button onClick={onClose} disabled={pending}>
              Cancel
            </Button>
            <Button variant="primary" data-autofocus onClick={onSave} disabled={pending}>
              {pending ? <Spinner /> : <Save aria-hidden="true" className="size-4" />}
              Save config.yaml
            </Button>
          </>
        )
      }
    >
      {moved && (
        <Alert tone="warn" title="config.yaml changed on the server since you opened it">
          <p>
            {base.etag !== undefined
              ? "Copy your edits and reload the latest file before saving. Version checks refuse this save so it cannot overwrite the changes made meanwhile."
              : "Saving also undoes what changed meanwhile. This save has no version guard. Copy your edits and reload the latest file before saving."}
          </p>
        </Alert>
      )}
      {same ? (
        <p>Nothing to save: the server&apos;s config.yaml is already the same.</p>
      ) : (
        <>
          <p className="text-muted">
            Saving replaces the whole file: {plural(added, "line", "lines")} added and{" "}
            {plural(removed, "line", "lines")} removed. The server checks the file first and keeps
            the old one if it can&apos;t use the new one.
          </p>
          {added === 0 && removed === 0 && (
            <p>Only the line endings or the end of the file differ.</p>
          )}
          <DiffView lines={lines} />
        </>
      )}
      {error !== null && <SaveProblem error={error} draft={review?.draft ?? ""} onReload={onReload} />}
    </Dialog>
  );
}

interface ConfigFileProps {
  initial: ConfigRead<string>;
  editor: ConfigEditor;
  onReload: () => void;
  onStored: (text: ConfigRead<string>) => void;
  onUnsavedChange?: ((unsaved: boolean) => void) | undefined;
}

/** The file, in an editor, once loaded. */
function ConfigFile({ initial, editor, onReload, onStored, onUnsavedChange }: ConfigFileProps) {
  const client = useQueryClient();
  const statusId = useId();
  /** The file as the editor opened it, or as last saved. */
  const [base, setBase] = useState(initial);
  const [draft, setDraft] = useState(initial.data);
  const [seenInitial, setSeenInitial] = useState(initial);
  // Counts the editor's fresh starts; Discard starts it again from `base`.
  const [round, setRound] = useState(0);
  const [review, setReview] = useState<Review | null>(null);
  const [saved, setSaved] = useState(false);
  const [readAfterSaveFailed, setReadAfterSaveFailed] = useState(false);
  const [confirmReload, setConfirmReload] = useState(false);
  const edited = !readAfterSaveFailed && draft !== base.data;

  useEffect(() => {
    onUnsavedChange?.(edited);
  }, [edited, editor, onUnsavedChange]);

  // A clean editor follows query updates; a draft retains its own base.
  if (!edited && initial !== seenInitial) {
    setSeenInitial(initial);
    setBase(initial);
    setDraft(initial.data);
    setRound((value) => value + 1);
  }

  const read = useMutation({
    mutationFn: async () => {
      const fresh = await editor.readValue<unknown>(CONFIG_YAML);
      if (typeof fresh.data !== "string") {
        throw new NotTextError();
      }
      return { fresh: fresh as ConfigRead<string>, draft };
    },
    onSuccess: setReview,
  });
  const save = useMutation({
    mutationFn: async (text: string) => {
      await editor.saveYaml(base, { method: "PUT", body: text, contentType: "application/yaml" });
      // Submission succeeded even if retrieving the normalized bytes fails.
      try {
        const stored = await editor.readValue<unknown>(CONFIG_YAML);
        if (typeof stored.data !== "string") throw new NotTextError();
        return stored as ConfigRead<string>;
      } catch { return null; }
    },
    onSuccess: (stored) => {
      setReview(null);
      setSaved(true);
      setReadAfterSaveFailed(stored === null);
      if (stored !== null) {
        onStored(stored);
        setBase(stored);
        setDraft(stored.data);
        setRound((value) => value + 1);
      }
    },
    // Every screen reads settings from it.
    onSettled: () => client.invalidateQueries(),
  });

  const closeReview = () => {
    setReview(null);
    save.reset();
  };

  return (
    <div className="space-y-3">
      {saved && !edited && !readAfterSaveFailed && (
        <Alert tone="ok" live>
          <p>Saved config.yaml. The server uses it from now on.</p>
        </Alert>
      )}
      {read.isError && <ProblemNotice problem={callProblem(read.error)} live onReloadConfig={onReload} />}
      {readAfterSaveFailed && <Alert tone="warn" live title="Saved config.yaml, but couldn't re-read it">
        <p>The file was saved. Reload to read the stored text before editing again.</p>
        <Button onClick={onReload}>Reload</Button>
      </Alert>}
      {!readAfterSaveFailed && <Suspense fallback={<Loading>Loading the editor…</Loading>}>
        <YamlEditor
          key={round}
          initial={base.data}
          label="config.yaml"
          onChange={(text) => {
            setSaved(false);
            setDraft(text);
          }}
        />
      </Suspense>}
      <div className="flex flex-wrap items-center justify-between gap-3">
        <p id={statusId} role="status" className="font-medium">
          {edited ? "Unsaved changes." : "No unsaved changes."}
        </p>
        <div className="flex flex-wrap gap-2">
          <Button
            onClick={() => {
              if (edited) {
                setConfirmReload(true);
              } else {
                onReload();
              }
            }}
          >
            <RotateCw aria-hidden="true" className="size-4" />
            Reload
          </Button>
          <Button
            disabled={!edited}
            onClick={() => {
              setBase(initial);
              setDraft(initial.data);
              setSeenInitial(initial);
              setRound((value) => value + 1);
            }}
          >
            <Undo2 aria-hidden="true" className="size-4" />
            Discard changes
          </Button>
          <Button
            variant="primary"
            disabled={!edited || read.isPending || readAfterSaveFailed}
            onClick={() => {
              setSaved(false);
              save.reset();
              read.mutate();
            }}
          >
            {read.isPending ? <Spinner /> : <FileDiff aria-hidden="true" className="size-4" />}
            Review changes
          </Button>
        </div>
      </div>
      <ReviewDialog
        review={review}
        base={base}
        pending={save.isPending}
        error={save.error}
        onSave={() => {
          if (review !== null) {
            save.mutate(review.draft);
          }
        }}
        onClose={closeReview}
        onReload={onReload}
      />
      <ConfirmDialog
        open={confirmReload}
        title="Discard your changes and reload?"
        confirmLabel="Discard and reload"
        onConfirm={() => {
          setConfirmReload(false);
          onReload();
        }}
        onCancel={() => {
          setConfirmReload(false);
        }}
      >
        <p>The editor shows config.yaml as the server has it now, without your changes.</p>
      </ConfirmDialog>
    </div>
  );
}

/**
 * config.yaml itself, for the settings the form doesn't have. It holds keys
 * in plain text, so it shows only when asked.
 */
export function ConfigFileEditor({
  onUnsavedChange,
}: {
  /** Told whether the editor holds unsaved changes, as that changes. */
  onUnsavedChange?: (unsaved: boolean) => void;
}) {
  const editor = useConfigEditor();
  const client = useQueryClient();
  const [visible, setVisible] = useState(false);
  const [round, setRound] = useState(0);
  const file = useQuery({
    queryKey: [CONFIG_YAML, "config-editor", editor.scope],
    enabled: visible,
    queryFn: async () => {
      const snapshot = await editor.editor.readValue<unknown>(CONFIG_YAML);
      if (typeof snapshot.data !== "string") throw new NotTextError();
      return snapshot as ConfigRead<string>;
    },
  });
  const reload = async () => {
    const result = await file.refetch();
    if (result.error === null) setRound((value) => value + 1);
  };

  return (
    <Card
      title="config.yaml"
      description="The whole file, for the settings the form doesn't have. Changes are checked and saved as a whole."
    >
      {file.data === undefined ? (
        <>
          <p>
            It holds the client keys and the provider API keys in plain text. Open it where no one
            can see your screen.
          </p>
          {file.isError && <ProblemNotice problem={callProblem(file.error)} live />}
          <Button
            variant="primary"
            disabled={file.isFetching}
            onClick={() => {
              setVisible(true);
              if (visible) void file.refetch();
            }}
          >
            {file.isFetching ? <Spinner /> : <Eye aria-hidden="true" className="size-4" />}
            Show config.yaml
          </Button>
        </>
      ) : (
        <>
          {file.data.recovery !== null && (
            <Alert tone="warn" live title="Recovery mode: the server can't use config.yaml">
              <p>The server is still running its last good config. Saving replaces the whole file without version checks. Copy your edits before reloading if the file changes meanwhile.</p>
              {file.data.recovery.detail !== null && <p>{file.data.recovery.detail}</p>}
            </Alert>
          )}
          <p className="text-muted">
            <Code>Ctrl</Code>+<Code>F</Code> (<Code>⌘</Code>+<Code>F</Code> on a Mac) searches.{" "}
            <Code>Tab</Code> leaves the editor, so indent with spaces.
          </p>
          {file.isFetching && <Loading>Reloading config.yaml…</Loading>}
          {file.isError && <ProblemNotice problem={callProblem(file.error)} live />}
          <ConfigFile
            key={round}
            initial={file.data}
            editor={editor.editor}
            onStored={(text) => { client.setQueryData([CONFIG_YAML, "config-editor", editor.scope], text); }}
            onReload={() => { void reload(); }}
            onUnsavedChange={onUnsavedChange}
          />
        </>
      )}
    </Card>
  );
}
