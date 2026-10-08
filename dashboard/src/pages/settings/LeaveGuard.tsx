import { useEffect } from "react";
import { useBlocker } from "react-router";

import { Button } from "../../components/Button";
import { Dialog } from "../../components/Dialog";
import { useReportUnsaved } from "../../layout/unsavedChanges";

export interface LeaveGuardProps {
  /** Whether anything on the page is unsaved. */
  unsaved: boolean;
}

/**
 * Asks before leaving a page with unsaved changes: for a link in the
 * dashboard, here; for a reload, a closed tab or another site, the
 * browser's own question; for signing out, the frame's. Moving within the
 * page, as between its tabs, doesn't ask.
 */
export function LeaveGuard({ unsaved }: LeaveGuardProps) {
  useReportUnsaved(unsaved);
  const blocker = useBlocker(
    ({ currentLocation, nextLocation }) =>
      unsaved && currentLocation.pathname !== nextLocation.pathname,
  );

  useEffect(() => {
    if (!unsaved) {
      return;
    }
    const ask = (event: BeforeUnloadEvent) => {
      event.preventDefault();
    };
    window.addEventListener("beforeunload", ask);
    return () => {
      window.removeEventListener("beforeunload", ask);
    };
  }, [unsaved]);

  // A move still held once nothing is unsaved would never go: drop it.
  useEffect(() => {
    if (blocker.state === "blocked" && !unsaved) {
      blocker.reset();
    }
  }, [blocker, unsaved]);

  const stay = () => {
    if (blocker.state === "blocked") {
      blocker.reset();
    }
  };

  return (
    <Dialog
      open={blocker.state === "blocked"}
      title="Leave without saving?"
      onClose={stay}
      footer={
        <>
          <Button data-autofocus onClick={stay}>
            Stay
          </Button>
          <Button
            variant="danger"
            onClick={() => {
              if (blocker.state === "blocked") {
                blocker.proceed();
              }
            }}
          >
            Leave without saving
          </Button>
        </>
      }
    >
      <p>Your changes on this page aren&apos;t saved yet. If you leave, they&apos;re lost.</p>
    </Dialog>
  );
}
