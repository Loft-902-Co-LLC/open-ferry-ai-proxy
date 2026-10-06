import { useMutation, useQueryClient } from "@tanstack/react-query";
import { Eye, FileDiff, RotateCw, Save, Undo2 } from "lucide-react";
import { Suspense, lazy, useId, useState } from "react";

import { callProblem, saveProblem } from "../../api/access";
import { isApiError } from "../../api/client";
import { useApiCall } from "../../api/hooks";
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

/** Why the server didn't save the file, in its words where it gave them. */
function SaveProblem({ error }: { error: unknown }) {
  if (isApiError(error) && error.code === "invalid_yaml") {
    return (
      <Alert tone="danger" live title="That isn't valid YAML">
        <p>{error.detail ?? "The server couldn't read it."} Nothing was saved.</p>
      </Alert>
    );
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
  fresh: string;
  /** What saving writes. */
  draft: string;
}

interface ReviewDialogProps {
  review: Review | null;
  /** The file as it was when the editor opened it. */
  base: string;
  pending: boolean;
  error: unknown;
  onSave: () => void;
  onClose: () => void;
}

/** The diff a save makes against the file as the server has it now. */
function ReviewDialog({ review, base, pending, error, onSave, onClose }: ReviewDialogProps) {
  const lines = review === null ? [] : diffLines(review.fresh, review.draft);
  const { added, removed } = diffCounts(lines);
  const same = review !== null && review.fresh === review.draft;
  const moved = review !== null && review.fresh !== base;
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
            The changes below are against the file as it is now, so saving also undoes what
            changed meanwhile. To keep those changes, close this, copy your edits, and reload the
            file.
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
      {error !== null && <SaveProblem error={error} />}
    </Dialog>
  );
}

/** The file, in an editor, once loaded. */
function ConfigFile({ initial, onReload }: { initial: string; onReload: () => void }) {
  const call = useApiCall();
  const client = useQueryClient();
  const statusId = useId();
  /** The file as the editor opened it, or as last saved. */
  const [base, setBase] = useState(initial);
  const [draft, setDraft] = useState(initial);
  // Counts the editor's fresh starts; Discard starts it again from `base`.
  const [round, setRound] = useState(0);
  const [review, setReview] = useState<Review | null>(null);
  const [saved, setSaved] = useState(false);
  const [confirmReload, setConfirmReload] = useState(false);
  const edited = draft !== base;

  const read = useMutation({
    mutationFn: async () => {
      const fresh = await call<unknown>(CONFIG_YAML);
      if (typeof fresh !== "string") {
        throw new NotTextError();
      }
      return { fresh, draft };
    },
    onSuccess: setReview,
  });
  const save = useMutation({
    mutationFn: (text: string) =>
      call<unknown>(CONFIG_YAML, { method: "PUT", body: text, contentType: "application/yaml" }),
    onSuccess: (_answer, text) => {
      setBase(text);
      setReview(null);
      setSaved(true);
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
      {saved && !edited && (
        <Alert tone="ok" live>
          <p>Saved config.yaml. The server uses it from now on.</p>
        </Alert>
      )}
      {read.isError && <ProblemNotice problem={callProblem(read.error)} live />}
      <Suspense fallback={<Loading>Loading the editor…</Loading>}>
        <YamlEditor
          key={round}
          initial={base}
          label="config.yaml"
          onChange={(text) => {
            setSaved(false);
            setDraft(text);
          }}
        />
      </Suspense>
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
              setDraft(base);
              setRound((value) => value + 1);
            }}
          >
            <Undo2 aria-hidden="true" className="size-4" />
            Discard changes
          </Button>
          <Button
            variant="primary"
            disabled={!edited || read.isPending}
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
export function ConfigFileEditor() {
  const call = useApiCall();
  // The file as last loaded, and how many loads there were, so a reload
  // starts the editor afresh.
  const [file, setFile] = useState<{ text: string; load: number } | null>(null);
  const load = useMutation({
    mutationFn: async () => {
      const text = await call<unknown>(CONFIG_YAML);
      if (typeof text !== "string") {
        throw new NotTextError();
      }
      return text;
    },
    onSuccess: (text) => {
      setFile((previous) => ({ text, load: (previous?.load ?? 0) + 1 }));
    },
  });

  return (
    <Card
      title="config.yaml"
      description="The whole file, for the settings the form doesn't have. Changes are checked and saved as a whole."
    >
      {file === null ? (
        <>
          <p>
            It holds the client keys and the provider API keys in plain text. Open it where no one
            can see your screen.
          </p>
          {load.isError && <ProblemNotice problem={callProblem(load.error)} live />}
          <Button
            variant="primary"
            disabled={load.isPending}
            onClick={() => {
              load.mutate();
            }}
          >
            {load.isPending ? <Spinner /> : <Eye aria-hidden="true" className="size-4" />}
            Show config.yaml
          </Button>
        </>
      ) : (
        <>
          <p className="text-muted">
            <Code>Ctrl</Code>+<Code>F</Code> (<Code>⌘</Code>+<Code>F</Code> on a Mac) searches.{" "}
            <Code>Tab</Code> leaves the editor, so indent with spaces.
          </p>
          {load.isPending && <Loading>Reloading config.yaml…</Loading>}
          {load.isError && <ProblemNotice problem={callProblem(load.error)} live />}
          <ConfigFile
            key={file.load}
            initial={file.text}
            onReload={() => {
              load.mutate();
            }}
          />
        </>
      )}
    </Card>
  );
}
