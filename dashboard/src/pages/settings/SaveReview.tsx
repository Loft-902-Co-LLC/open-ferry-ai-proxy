import { CircleCheck, Info, Save, Undo2 } from "lucide-react";
import type { Ref } from "react";

import { callProblem, saveProblem } from "../../api/access";
import { Alert } from "../../components/Alert";
import { Badge } from "../../components/Badge";
import { Button } from "../../components/Button";
import { Code } from "../../components/Code";
import { Dialog } from "../../components/Dialog";
import { ProblemNotice } from "../../components/ProblemNotice";
import { Spinner } from "../../components/Spinner";
import { Table, Td, Th } from "../../components/Table";
import { DuplicateKeyError, KeyGoneError, keyChangeLabel, shownKey, type KeyChange } from "./keyChanges";
import { SETTINGS, describeSetting, type SettingChange } from "./settingsModel";

/** A change on the Settings tab that waits for "Review and save". */
export type PendingChange =
  | { kind: "key"; change: KeyChange }
  | { kind: "setting"; change: SettingChange };

/** A change's name, as the review and its notices give it. */
function changeName(pending: PendingChange): string {
  return pending.kind === "key" ? keyChangeLabel(pending.change) : SETTINGS[pending.change.id].label;
}

/** A save that stopped at a change the server refused. */
export class SaveStoppedError extends Error {
  constructor(
    readonly saved: PendingChange[],
    readonly failed: PendingChange,
    readonly reason: unknown,
  ) {
    super("save stopped");
    this.name = "SaveStoppedError";
  }
}

export function plural(count: number, one: string, many: string): string {
  return `${count.toLocaleString("en")} ${count === 1 ? one : many}`;
}

/** "3 settings" when every change is a setting, else "3 changes". */
export function countChanges(count: number, settingsOnly: boolean): string {
  return settingsOnly ? plural(count, "setting", "settings") : plural(count, "change", "changes");
}

/** What a save will do, read against the server just before it. */
export interface Review {
  /** In the order they are saved: client keys first, then settings. */
  changes: PendingChange[];
  /** How many client keys are left once saved, when keys change. */
  keysAfter: number | null;
}

/** Why a change wasn't saved. */
function StopReason({ reason }: { reason: unknown }) {
  if (reason instanceof DuplicateKeyError) {
    return (
      <Alert tone="warn" live title="That key is already in the list">
        <p>Something else added it meanwhile, so it wasn&apos;t added again.</p>
      </Alert>
    );
  }
  if (reason instanceof KeyGoneError) {
    return (
      <Alert tone="info" live title="That key was already deleted">
        <p>Something else deleted it meanwhile.</p>
      </Alert>
    );
  }
  return <ProblemNotice problem={saveProblem(reason)} live />;
}

interface ReviewDialogProps {
  review: Review | null;
  pending: boolean;
  error: unknown;
  onSave: () => void;
  onClose: () => void;
}

/**
 * What a save will change: client keys to add and delete, and each setting
 * against the server's value now.
 */
export function ReviewDialog({ review, pending, error, onSave, onClose }: ReviewDialogProps) {
  const changes = review?.changes ?? [];
  const keys = changes.flatMap((pending) => (pending.kind === "key" ? [pending.change] : []));
  const settings = changes.flatMap((pending) => (pending.kind === "setting" ? [pending.change] : []));
  const settingsOnly = keys.length === 0;
  const both = keys.length > 0 && settings.length > 0;
  const lastKey = review?.keysAfter === 0 && keys.some((change) => change.kind === "delete");
  const moved = settings.some((change) => change.movedOnServer);
  const stopped = error instanceof SaveStoppedError ? error : null;
  const total = changes.length;
  const notTried = stopped === null ? 0 : total - stopped.saved.length - 1;
  return (
    <Dialog
      open={review !== null}
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
              Save {countChanges(total, settingsOnly)}
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
        Each change is saved to config.yaml on its own, in this order, and the server uses it from
        then on. Nothing else in the file changes.
      </p>
      {keys.length > 0 && (
        <div className="space-y-2">
          {both && <h3 className="font-semibold">Client keys</h3>}
          <ul aria-label="The client key changes" className="divide-y divide-line border-y border-line">
            {keys.map((change, index) => (
              <li key={`${change.kind}-${change.key}-${String(index)}`} className="py-1.5">
                {change.kind === "add" ? "Add" : "Delete"} client key <Code>{shownKey(change.key)}</Code>
              </li>
            ))}
          </ul>
          <p className="text-muted">
            A new key works, and a deleted one stops working, as soon as it is saved.
          </p>
          {lastKey && (
            <Alert tone="warn" title="This deletes the last client key">
              <p>
                With none, the proxy takes requests from anyone who can reach it, with no key at
                all.
              </p>
            </Alert>
          )}
        </div>
      )}
      {settings.length > 0 && (
        <div className="space-y-2">
          {both && <h3 className="font-semibold">Settings</h3>}
          {moved && (
            <Alert tone="warn" title="Some of these changed on the server since the page loaded">
              <p>
                Someone or something else changed them. &ldquo;Now&rdquo; shows the server&apos;s
                values as they are now; saving replaces them.
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
              {settings.map((change) => (
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
                  <Td className="break-all font-medium">
                    {describeSetting(change.id, change.after)}
                  </Td>
                </tr>
              ))}
            </tbody>
          </Table>
        </div>
      )}
      {error !== null && (
        <>
          {stopped !== null && stopped.saved.length > 0 && (
            <Alert tone="warn" live title="Saved some of the changes">
              <p>
                Saved {stopped.saved.length} of {total}. &ldquo;{changeName(stopped.failed)}
                &rdquo; wasn&apos;t saved
                {notTried > 0
                  ? `, nor the ${countChanges(notTried, settingsOnly)} after it`
                  : ""}
                .
              </p>
            </Alert>
          )}
          <StopReason reason={stopped?.reason ?? error} />
        </>
      )}
    </Dialog>
  );
}

/** What the last "Review and save" did. */
export type Outcome = { saved: number; settingsOnly: boolean } | "nothing" | null;

interface SaveBarProps {
  ref?: Ref<HTMLDivElement>;
  /** The changes waiting on the tab: settings edited, and client keys added or deleted. */
  unsaved: number;
  outcome: Outcome;
  /** Why reading the server for the review failed, if it did. */
  reviewError: unknown;
  reviewing: boolean;
  /**
   * The settings form, when it shows: "Review and save" submits it, so Enter
   * in a field starts the review too. Else the button calls `onReview`.
   */
  formId?: string | undefined;
  onDiscard: () => void;
  onReview: () => void;
}

/** The bar at the foot of the Settings tab: what is unsaved, and the one way to save it. */
export function SaveBar({
  ref,
  unsaved,
  outcome,
  reviewError,
  reviewing,
  formId,
  onDiscard,
  onReview,
}: SaveBarProps) {
  return (
    <div
      ref={ref}
      className="sticky bottom-0 z-10 -mx-1 space-y-3 rounded-t-lg border border-line bg-surface px-4 py-3 shadow-lg"
    >
      {/* A line, not a box: the bar is a box already. */}
      {outcome !== null && outcome !== "nothing" && (
        <p role="status" className="flex items-start gap-2">
          <CircleCheck aria-hidden="true" className="mt-0.5 size-4 shrink-0 text-ok" />
          <span>
            Saved {countChanges(outcome.saved, outcome.settingsOnly)}. The server uses{" "}
            {outcome.saved === 1 ? "it" : "them"} from now on.
          </span>
        </p>
      )}
      {outcome === "nothing" && (
        <p role="status" className="flex items-start gap-2">
          <Info aria-hidden="true" className="mt-0.5 size-4 shrink-0 text-accent" />
          <span>Nothing to save: the server already has these values.</span>
        </p>
      )}
      {reviewError !== null && <ProblemNotice problem={callProblem(reviewError)} live />}
      <div className="flex flex-wrap items-center justify-between gap-3">
        <p role="status" className="font-medium">
          {unsaved === 0
            ? "No unsaved changes."
            : `${plural(unsaved, "unsaved change", "unsaved changes")}.`}
        </p>
        <div className="flex flex-wrap gap-2">
          <Button disabled={unsaved === 0 || reviewing} onClick={onDiscard}>
            <Undo2 aria-hidden="true" className="size-4" />
            Discard
          </Button>
          <Button
            type={formId === undefined ? "button" : "submit"}
            form={formId}
            variant="primary"
            disabled={unsaved === 0 || reviewing}
            onClick={formId === undefined ? onReview : undefined}
          >
            {reviewing ? <Spinner /> : <Save aria-hidden="true" className="size-4" />}
            Review and save
          </Button>
        </div>
      </div>
    </div>
  );
}
