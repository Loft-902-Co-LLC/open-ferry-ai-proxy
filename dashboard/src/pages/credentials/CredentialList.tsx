import { useState, type ReactNode } from "react";

import { AUTH_FILES, type Credential, type CredentialList as List } from "../../api/credentials";
import { useApiQuery } from "../../api/hooks";
import { Alert } from "../../components/Alert";
import { Card } from "../../components/Card";
import { credentialAnchor } from "./anchors";
import { CredentialItem, credentialNames } from "./CredentialItem";
import { credentialHealth, secondsUntilBack } from "./credentialStates";
import { CheckedAt, PolledState } from "./PolledState";
import { TriageList, type TriageEntry } from "./TriageList";
import { useUpload } from "./upload";

/** How often the list is read again, for its health and cooldowns. */
export const CREDENTIALS_REFRESH_MS = 15_000;

/**
 * A credential list's `refetchInterval`: every CREDENTIALS_REFRESH_MS once
 * it has been read. One that failed, or that the server doesn't serve, isn't
 * asked for again on a timer: each try would show "Loading" in its place
 * again. Its error offers to try again instead.
 */
export function pollWhileRead(query: { state: { data: unknown } }): number | false {
  return query.state.data === undefined ? false : CREDENTIALS_REFRESH_MS;
}

interface Entry extends TriageEntry {
  credential: Credential;
}

function entryOf(credential: Credential, name: string): Entry {
  return {
    key: credential.id,
    anchor: credentialAnchor(credential.id),
    name,
    health: credentialHealth(credential),
    backIn: secondsUntilBack(credential),
    credential,
  };
}

/**
 * The credential files and sign-ins the server has: the failing and
 * resting ones first, in full, the rest folded away.
 */
export function CredentialList() {
  const list = useApiQuery<List>(AUTH_FILES, undefined, { refetchInterval: pollWhileRead });
  const upload = useUpload();
  const [notice, setNotice] = useState<ReactNode>(null);

  return (
    <Card
      title="Sign-ins and credential files"
      description={
        <>
          The accounts the server signs in to providers with. Any that need you come first.
          <CheckedAt at={list.dataUpdatedAt} />
        </>
      }
      actions={upload.button}
    >
      {upload.result}
      {notice !== null && (
        <Alert tone="ok" live>
          <p>{notice}</p>
        </Alert>
      )}
      <PolledState query={list} loading="Loading the credentials…">
        {(data) => {
          const files = data.files ?? [];
          if (files.length === 0) {
            return (
              <p className="text-muted">
                None yet. Sign in with Claude or ChatGPT, upload credential files, or add a
                provider API key below.
              </p>
            );
          }
          const names = credentialNames(files);
          return (
            <TriageList
              entries={files.map((credential, index) =>
                entryOf(credential, names[index] ?? credential.name),
              )}
              render={(entry, view) => (
                <CredentialItem
                  credential={entry.credential}
                  name={entry.name}
                  anchor={entry.anchor}
                  compact={view.compact}
                  targeted={view.targeted}
                  onDone={setNotice}
                />
              )}
            />
          );
        }}
      </PolledState>
    </Card>
  );
}
