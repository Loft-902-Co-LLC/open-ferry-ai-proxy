import { useMutation, useQuery, useQueryClient, type UseQueryResult } from "@tanstack/react-query";
import {
  useEffect,
  useId,
  useRef,
  useState,
  type Dispatch,
  type SetStateAction,
} from "react";

import { callProblem, cantSaveConfig } from "../../api/access";
import { isUnsupportedRoute } from "../../api/client";
import { useApiCall, useApiQuery } from "../../api/hooks";
import { API_KEYS, CONFIG, V8_CONFIG } from "../../api/management";
import { Alert } from "../../components/Alert";
import { Loading, QueryState } from "../../components/QueryState";
import type { ApiKeysAnswer } from "../overview/clientKeys";
import { ClientKeysCard } from "./ClientKeysCard";
import {
  applyKeyChanges,
  effectiveKeyChanges,
  keysIn,
  saveKeyChange,
  withoutChange,
  type KeyChange,
} from "./keyChanges";
import {
  ReviewDialog,
  SaveBar,
  SaveStoppedError,
  type Outcome,
  type PendingChange,
  type Review,
} from "./SaveReview";
import { SettingsForm, useSettingsForm } from "./SettingsForm";
import { managementAddressProblem, restartNotice } from "./managementAddress";
import {
  SETTING_IDS,
  readServerFacts,
  saveCall,
  settingChanges,
  settingValuesOf,
  type ServerFacts,
  type SettingValues,
} from "./settingsModel";

/** The query key of what the tab reads through the v8 config route. */
const FACTS = [V8_CONFIG, "settings"] as const;

/** Whether `config`, an answer of `GET /config`, has TLS on (`tls.enable`). */
function tlsOn(config: unknown): boolean {
  if (config === null || typeof config !== "object") {
    return false;
  }
  const tls = (config as Record<string, unknown>).tls;
  return tls !== null && typeof tls === "object" && (tls as Record<string, unknown>).enable === true;
}

/** What "Review and save" reads the server for. */
interface ReviewRequest {
  /** The settings as edited, once checked, or null when none are. */
  edited: SettingValues | null;
  /** The settings as the page loaded them, which the edits are against. */
  base: SettingValues;
  /** The client keys to add and delete. */
  keys: KeyChange[];
}

interface SettingsEditorProps {
  config: UseQueryResult;
  facts: UseQueryResult<ServerFacts>;
  keys: UseQueryResult<ApiKeysAnswer>;
  keyChanges: KeyChange[];
  setKeyChanges: Dispatch<SetStateAction<KeyChange[]>>;
  keysReadOnly: boolean;
  onKeysReadOnly: () => void;
  onUnsavedChange: (unsaved: boolean) => void;
}

/**
 * The tab once it knows whether the settings loaded: the client keys and the
 * settings form, with one bar and one review for the changes to both.
 */
function SettingsEditor({
  config,
  facts,
  keys,
  keyChanges,
  setKeyChanges,
  keysReadOnly,
  onKeysReadOnly,
  onUnsavedChange,
}: SettingsEditorProps) {
  const call = useApiCall();
  const client = useQueryClient();
  const formId = useId();
  const root = useRef<HTMLDivElement>(null);
  const bar = useRef<HTMLDivElement>(null);
  const settingsUnsupported = isUnsupportedRoute(config.error);
  // The form waits for the management address too, so it never counts it
  // arriving as an edit. A server without the v8 config route has no field
  // for it.
  const factsUnsupported = isUnsupportedRoute(facts.error);
  const factsRead = facts.dataUpdatedAt > 0 || facts.errorUpdatedAt > 0;
  const settingsShown = config.data !== undefined && !settingsUnsupported && factsRead;
  const settings = useSettingsForm(settingsShown ? config.data : undefined, facts.data);
  const savedKeys = keysIn(keys.data);
  const pendingKeys = keys.data === undefined ? [] : effectiveKeyChanges(savedKeys, keyChanges);
  const unsaved = (settingsShown ? settings.unsaved.length : 0) + pendingKeys.length;
  const [review, setReview] = useState<Review | null>(null);
  const [outcome, setOutcome] = useState<Outcome>(null);

  useEffect(() => {
    onUnsavedChange(unsaved > 0);
  }, [unsaved, onUnsavedChange]);

  // A field scrolled into view, as by Tab, stops clear of the bar, with room
  // for the start of its hint: its scroll margin is the bar's height and
  // 3rem, kept up to date as the bar grows.
  useEffect(() => {
    const host = root.current;
    const element = bar.current;
    if (host === null || element === null) {
      return;
    }
    const observer = new ResizeObserver(() => {
      host.style.setProperty("--save-bar-space", `calc(${String(element.offsetHeight)}px + 3rem)`);
    });
    observer.observe(element);
    return () => {
      observer.disconnect();
    };
  }, []);

  /** Marks `saved` as saved: the form and the key list take them as the server's. */
  const settleSaved = (saved: readonly PendingChange[]) => {
    const keysSaved: KeyChange[] = [];
    for (const pending of saved) {
      if (pending.kind === "setting") {
        settings.settle(pending.change.id, pending.change.after);
      } else {
        keysSaved.push(pending.change);
      }
    }
    if (keysSaved.length === 0) {
      return;
    }
    client.setQueryData<ApiKeysAnswer>([API_KEYS], (answer) =>
      answer === undefined
        ? answer
        : { ...answer, "api-keys": applyKeyChanges(keysIn(answer), keysSaved) },
    );
    setKeyChanges((current) =>
      keysSaved.reduce((left, change) => withoutChange(left, change.kind, change.key), current),
    );
  };

  const read = useMutation({
    mutationFn: async ({ edited, base, keys: changes }: ReviewRequest) => {
      let fresh: SettingValues | null = null;
      let restart: string | null = null;
      let addressProblem: string | null = null;
      if (edited !== null) {
        const answer = await call<unknown>(CONFIG);
        client.setQueryData([CONFIG], answer);
        let freshFacts = facts.data;
        if (edited.managementAddress !== base.managementAddress) {
          // The proxy's port may have moved since the page read it.
          freshFacts = await readServerFacts(call);
          client.setQueryData(FACTS, freshFacts);
          addressProblem = managementAddressProblem(edited.managementAddress, freshFacts.proxyPort);
          restart = restartNotice(edited.managementAddress, {
            tls: tlsOn(answer),
            proxyPort: freshFacts.proxyPort,
            origin: window.location.origin,
          });
        }
        fresh = settingValuesOf(answer, freshFacts);
      }
      let freshKeys: string[] | null = null;
      if (changes.length > 0) {
        const answer = await call<unknown>(API_KEYS);
        client.setQueryData([API_KEYS], answer);
        freshKeys = keysIn(answer);
      }
      return { edited, base, changes, fresh, freshKeys, restart, addressProblem };
    },
    onSuccess: ({ edited, base, changes, fresh, freshKeys, restart, addressProblem }) => {
      if (addressProblem !== null) {
        // Shown at the field, as any problem with an edit is.
        settings.form.setError(
          "managementAddress",
          { type: "validate", message: addressProblem },
          { shouldFocus: true },
        );
        return;
      }
      const found: PendingChange[] = [];
      let keysAfter: number | null = null;
      if (freshKeys !== null) {
        // A key added or deleted elsewhere meanwhile is no longer a change.
        const keysFound = effectiveKeyChanges(freshKeys, changes);
        setKeyChanges((current) => effectiveKeyChanges(freshKeys, current));
        found.push(...keysFound.map((change): PendingChange => ({ kind: "key", change })));
        keysAfter = applyKeyChanges(freshKeys, keysFound).length;
      }
      let restartFound: string | null = null;
      if (edited !== null && fresh !== null) {
        // An edit the server already has is no longer one.
        for (const id of SETTING_IDS) {
          if (edited[id] === fresh[id]) {
            settings.settle(id, fresh[id]);
          }
        }
        const settingsFound = settingChanges(base, edited, fresh);
        found.push(...settingsFound.map((change): PendingChange => ({ kind: "setting", change })));
        if (settingsFound.some((change) => change.id === "managementAddress")) {
          restartFound = restart;
        }
      }
      if (found.length === 0) {
        setOutcome("nothing");
        return;
      }
      setReview({ changes: found, keysAfter, restart: restartFound });
    },
  });

  const save = useMutation({
    mutationFn: async (toSave: readonly PendingChange[]) => {
      const saved: PendingChange[] = [];
      for (const pending of toSave) {
        try {
          if (pending.kind === "key") {
            await saveKeyChange(call, pending.change);
          } else {
            const { path, request } = saveCall(pending.change.id, pending.change.after);
            await call<unknown>(path, request);
          }
        } catch (reason) {
          throw new SaveStoppedError(saved, pending, reason);
        }
        saved.push(pending);
      }
      return saved;
    },
    onSuccess: (saved) => {
      settleSaved(saved);
      setReview(null);
      setOutcome({
        saved: saved.length,
        settingsOnly: saved.every((pending) => pending.kind === "setting"),
        restart: saved.some(
          (pending) => pending.kind === "setting" && pending.change.id === "managementAddress",
        ),
      });
    },
    onError: (error) => {
      if (!(error instanceof SaveStoppedError)) {
        return;
      }
      settleSaved(error.saved);
      const { failed, reason } = error;
      // The keys are in config.yaml too: when the server can't save the
      // file, the card says so instead of offering changes it can't keep.
      if (
        cantSaveConfig(reason) &&
        (failed.kind === "key" || callProblem(reason).kind === "config-not-saved")
      ) {
        onKeysReadOnly();
      }
    },
    // Other screens show some of these settings, and the keys, too.
    onSettled: () => client.invalidateQueries(),
  });

  const startReview = () => {
    setOutcome(null);
    read.reset();
    save.reset();
    const request = { base: settings.loaded, keys: pendingKeys };
    if (settingsShown && settings.unsaved.length > 0) {
      // Checks the edited settings first; a problem shows at its field.
      void settings.form.handleSubmit((edited) => {
        read.mutate({ ...request, edited });
      })();
      return;
    }
    read.mutate({ ...request, edited: null });
  };

  return (
    <div ref={root} className="space-y-4 [--save-bar-space:10rem]">
      <div className="space-y-4 **:scroll-mb-(--save-bar-space)">
        <ClientKeysCard
          keys={keys}
          changes={keyChanges}
          onChanges={(next) => {
            setKeyChanges(effectiveKeyChanges(savedKeys, next));
          }}
          readOnly={keysReadOnly}
        />
        {settingsUnsupported ? (
          <Alert tone="info" title="Settings can't be changed here">
            <p>
              This server doesn&apos;t serve its settings to the dashboard. Change them in
              config.yaml itself: the server picks the change up when it reloads the file.
            </p>
          </Alert>
        ) : settingsShown ? (
          <SettingsForm
            id={formId}
            settings={settings}
            facts={factsUnsupported ? null : facts}
            onSubmit={startReview}
          />
        ) : config.data === undefined ? (
          <QueryState query={config} loading="Reading the settings…">
            {() => null}
          </QueryState>
        ) : (
          <Loading>Reading the settings…</Loading>
        )}
      </div>
      <SaveBar
        ref={bar}
        unsaved={unsaved}
        outcome={outcome}
        reviewError={read.error}
        reviewing={read.isPending}
        formId={settingsShown ? formId : undefined}
        onDiscard={() => {
          settings.discard();
          setKeyChanges([]);
          setOutcome(null);
          read.reset();
        }}
        onReview={startReview}
      />
      <ReviewDialog
        review={review}
        pending={save.isPending}
        error={save.error}
        onSave={() => {
          if (review !== null) {
            save.mutate(review.changes);
          }
        }}
        onClose={() => {
          setReview(null);
          save.reset();
        }}
      />
    </div>
  );
}

export interface SettingsTabProps {
  /** Told whether anything on the tab is unsaved, as that changes. */
  onUnsavedChange: (unsaved: boolean) => void;
}

/**
 * The Settings tab: client keys and the common settings. Every change, a
 * key added or deleted as much as a setting edited, waits for "Review and
 * save".
 */
export function SettingsTab({ onUnsavedChange }: SettingsTabProps) {
  const config = useApiQuery<unknown>(CONFIG);
  const call = useApiCall();
  const facts = useQuery({
    queryKey: FACTS,
    queryFn: ({ signal }) => readServerFacts(call, signal),
  });
  const keys = useApiQuery<ApiKeysAnswer>(API_KEYS);
  // Kept here, so they outlast the editor starting afresh below.
  const [keyChanges, setKeyChanges] = useState<KeyChange[]>([]);
  const [keysReadOnly, setKeysReadOnly] = useState(false);
  return (
    // The form starts once, with the settings loaded, so it never counts
    // the values arriving as edits.
    <SettingsEditor
      key={
        config.data === undefined || (facts.dataUpdatedAt === 0 && facts.errorUpdatedAt === 0)
          ? "loading"
          : "loaded"
      }
      config={config}
      facts={facts}
      keys={keys}
      keyChanges={keyChanges}
      setKeyChanges={setKeyChanges}
      keysReadOnly={keysReadOnly}
      onKeysReadOnly={() => {
        setKeysReadOnly(true);
      }}
      onUnsavedChange={onUnsavedChange}
    />
  );
}
