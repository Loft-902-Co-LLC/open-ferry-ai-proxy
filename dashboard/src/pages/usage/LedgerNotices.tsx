import { USAGE_LEDGER, type LedgerState } from "../../api/dashboard";
import { USAGE_STATISTICS_ENABLED } from "../../api/management";
import { Alert } from "../../components/Alert";
import { Code } from "../../components/Code";
import { TurnOnSetting } from "../../components/TurnOnSetting";
import { formatInteger } from "../../lib/format";

/** What is wrong with the ledger, if anything, and what to do about it. */
export function LedgerNotices({ ledger }: { ledger: LedgerState }) {
  if (!ledger.available) {
    return (
      <Alert tone="danger" title="The usage ledger couldn't be opened">
        <p>
          The server keeps usage in <Code>open-ferry-usage.sqlite3</Code>, in its log directory
          (<Code>WRITABLE_PATH</Code>, else beside config.yaml), and couldn&apos;t open or create
          it:
        </p>
        <p className="font-mono break-words">{ledger.unavailable_reason ?? "no reason given"}</p>
        <p>Check that the directory exists and the server may write to it, then restart it.</p>
      </Alert>
    );
  }
  return (
    <>
      {!ledger.usage_statistics_enabled && (
        <Alert tone="warn" title="Usage isn't being recorded">
          <p>
            <Code>usage-statistics-enabled</Code> is off, so new calls aren&apos;t counted. What
            was recorded before stays.
          </p>
          <TurnOnSetting
            path={USAGE_STATISTICS_ENABLED}
            label="Start recording"
            configKey="usage-statistics-enabled"
            invalidate={[[USAGE_LEDGER]]}
          />
        </Alert>
      )}
      {(ledger.dropped_records ?? 0) > 0 && (
        <Alert tone="warn" title="Some calls weren't recorded">
          <p>
            The ledger fell behind and lost {formatInteger(ledger.dropped_records ?? 0)} records
            since the server started. A busy or slow disk can cause it; the totals here are low by
            that much.
          </p>
        </Alert>
      )}
    </>
  );
}
