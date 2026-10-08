import { useMutation, useQueryClient, type UseQueryResult } from "@tanstack/react-query";
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
import { API_KEYS, CONFIG } from "../../api/management";
import { Alert } from "../../components/Alert";
import { QueryState } from "../../components/QueryState";
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
import {
  SETTINGS,
  SETTING_IDS,
  settingChanges,
  settingValuesOf,
  type SettingValues,
} from "./settingsModel";

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
  keys: UseQueryResult<ApiKeysAnswer>;
  keyChanges: KeyChange[];
  setKeyChanges: Dispatch<SetStateAction<KeyChange[]>>;
  keysReadOnly: boolean;
  onKeysReadOnly: () => void;
}

/**
 * The tab once it knows whether the settings loaded: the client keys and the
 * settings form, with one bar and one review for the changes to both.
 */
function SettingsEditor({
  config,
  keys,
  keyChanges,
  setKeyChanges,
  keysReadOnly,
  onKeysReadOnly,
}: SettingsEditorProps) {
  const call = useApiCall();
  const client = useQueryClient();
  const formId = useId();
  const root = useRef<HTMLDivElement>(null);
  const bar = useRef<HTMLDivElement>(null);
  const settingsUnsupported = isUnsupportedRoute(config.error);
  const settingsShown = config.data !== undefined && !settingsUnsupported;
  const settings = useSettingsForm(settingsShown ? config.data : undefined);
  const savedKeys = keysIn(keys.data);
  const pendingKeys = keys.data === undefined ? [] : effectiveKeyChanges(savedKeys, keyChanges);
  const unsaved = (settingsShown ? settings.unsaved.length : 0) + pendingKeys.length;
  const [review, setReview] = useState<Review | null>(null);
  const [outcome, setOutcome] = useState<Outcome>(null);

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
      if (edited !== null) {
        const answer = await call<unknown>(CONFIG);
        client.setQueryData([CONFIG], answer);
        fresh = settingValuesOf(answer);
      }
      let freshKeys: string[] | null = null;
      if (changes.length > 0) {
        const answer = await call<unknown>(API_KEYS);
        client.setQueryData([API_KEYS], answer);
        freshKeys = keysIn(answer);
      }
      return { edited, base, changes, fresh, freshKeys };
    },
    onSuccess: ({ edited, base, changes, fresh, freshKeys }) => {
      const found: PendingChange[] = [];
      let keysAfter: number | null = null;
      if (freshKeys !== null) {
        // A key added or deleted elsewhere meanwhile is no longer a change.
        const keysFound = effectiveKeyChanges(freshKeys, changes);
        setKeyChanges((current) => effectiveKeyChanges(freshKeys, current));
        found.push(...keysFound.map((change): PendingChange => ({ kind: "key", change })));
        keysAfter = applyKeyChanges(freshKeys, keysFound).length;
      }
      if (edited !== null && fresh !== null) {
        // An edit the server already has is no longer one.
        for (const id of SETTING_IDS) {
          if (edited[id] === fresh[id]) {
            settings.settle(id, fresh[id]);
          }
        }
        found.push(
          ...settingChanges(base, edited, fresh).map(
            (change): PendingChange => ({ kind: "setting", change }),
          ),
        );
      }
      if (found.length === 0) {
        setOutcome("nothing");
        return;
      }
      setReview({ changes: found, keysAfter });
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
            await call<unknown>(SETTINGS[pending.change.id].path, {
              method: "PATCH",
              json: { value: pending.change.after },
            });
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
          <SettingsForm id={formId} settings={settings} onSubmit={startReview} />
        ) : (
          <QueryState query={config} loading="Reading the settings…">
            {() => null}
          </QueryState>
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

/**
 * The Settings tab: client keys and the common settings. Every change, a
 * key added or deleted as much as a setting edited, waits for "Review and
 * save".
 */
export function SettingsTab() {
  const config = useApiQuery<unknown>(CONFIG);
  const keys = useApiQuery<ApiKeysAnswer>(API_KEYS);
  // Kept here, so they outlast the editor starting afresh below.
  const [keyChanges, setKeyChanges] = useState<KeyChange[]>([]);
  const [keysReadOnly, setKeysReadOnly] = useState(false);
  return (
    // The form starts once, with the settings loaded, so it never counts
    // the values arriving as edits.
    <SettingsEditor
      key={config.data === undefined ? "loading" : "loaded"}
      config={config}
      keys={keys}
      keyChanges={keyChanges}
      setKeyChanges={setKeyChanges}
      keysReadOnly={keysReadOnly}
      onKeysReadOnly={() => {
        setKeysReadOnly(true);
      }}
    />
  );
}
