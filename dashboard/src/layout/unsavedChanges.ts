// Whether the page in view holds changes that aren't saved yet. Signing out
// leaves every page at once, without a move the page's own guard could
// stop, so the frame asks first when this is set.

import { createContext, useContext, useEffect } from "react";

/** Tells the frame whether the page in view has unsaved changes. */
export const UnsavedChangesContext = createContext<(unsaved: boolean) => void>(() => {
  // Outside the frame, as when a test renders a page alone, nobody asks.
});

/** Reports `unsaved` to the frame while the calling page is shown. */
export function useReportUnsaved(unsaved: boolean): void {
  const report = useContext(UnsavedChangesContext);
  useEffect(() => {
    report(unsaved);
    return () => {
      report(false);
    };
  }, [report, unsaved]);
}
