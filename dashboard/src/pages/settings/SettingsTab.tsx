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
import { useConfigEditor, useConfigQuery, useDraftConfigQuery } from "../../api/hooks";
import type { ConfigRead } from "../../api/configEditor";
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
  serverFactsIn,
  type SettingId,
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
  editor: ReturnType<typeof useConfigEditor>;
  config: UseQueryResult & { snapshot: ConfigRead<unknown> | undefined };
  facts: UseQueryResult<ServerFacts> & { snapshot: ConfigRead<unknown> | undefined };
  keys: UseQueryResult<ApiKeysAnswer>;
  keyChanges: KeyChange[];
  setKeyChanges: Dispatch<SetStateAction<KeyChange[]>>;
  keysReadOnly: boolean;
  onKeysReadOnly: () => void;
  onUnsavedChange: (unsaved: boolean) => void;
  onReload: () => void;
}

/**
 * The tab once it knows whether the settings loaded: the client keys and the
 * settings form, with one bar and one review for the changes to both.
 */
function SettingsEditor({
  editor,
  config,
  facts,
  keys,
  keyChanges,
  setKeyChanges,
  keysReadOnly,
  onKeysReadOnly,
  onUnsavedChange,
  onReload,
}: SettingsEditorProps) {
  const { call } = editor;
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
  const pendingKeys = keys.data === undefined ? [] : editor.editor.versioned
    ? [...effectiveKeyChanges(savedKeys, keyChanges).filter((change) => change.kind === "add"),
      ...keyChanges.filter((change) => change.kind === "delete")]
    : effectiveKeyChanges(savedKeys, keyChanges);
  const unsaved = (settingsShown ? settings.unsaved.length : 0) + pendingKeys.length;
  const [review, setReview] = useState<Review | null>(null);
  const [outcome, setOutcome] = useState<Outcome>(null);

  useEffect(() => {
    editor.setDirty(unsaved > 0);
    editor.editor.retainDrafts(settings.retained.map((id) => saveCall(id, settings.loaded[id]).path));
    onUnsavedChange(unsaved > 0);
  }, [editor, unsaved, settings.retained, settings.loaded, onUnsavedChange]);

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
    const accepted = editor.editor.acceptedValue<unknown>(CONFIG);
    if (saved.length > 0 && accepted !== undefined) {
      // Publish the acknowledged values/version together before clearing
      // RHF's draft, even while the background read is pending.
      client.setQueryData([CONFIG, "config-draft", editor.scope], accepted);
      client.setQueryData([...FACTS, editor.scope], {
        data: serverFactsIn(accepted.tree), snapshot: { ...accepted, data: accepted.tree },
      });
    }
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
    client.setQueryData<ApiKeysAnswer>([API_KEYS, "config-editor", editor.scope], (answer) =>
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
      await editor.checkChanges([
        ...(edited === null ? [] : SETTING_IDS.filter((id) => edited[id] !== base[id])
          .map((id) => saveCall(id, edited[id]).path)),
      ]);
      let fresh: SettingValues | null = null;
      let restart: string | null = null;
      let addressProblem: string | null = null;
      const moved = new Set<SettingId>();
      if (edited !== null) {
        const snapshot = await editor.editor.readValue<unknown>(CONFIG);
        const answer = snapshot.data;
        client.setQueryData([CONFIG, "config-draft", editor.scope], snapshot);
        let freshFacts = facts.data;
        let addressSnapshot = snapshot;
        if (edited.managementAddress !== base.managementAddress) {
          // The proxy's port may have moved since the page read it.
          const factsSnapshot = await editor.editor.readValue<unknown>(V8_CONFIG);
          addressSnapshot = factsSnapshot;
          freshFacts = await readServerFacts(() => Promise.resolve(factsSnapshot.data));
          client.setQueryData([...FACTS, editor.scope], { data: freshFacts, snapshot: factsSnapshot });
          addressProblem = managementAddressProblem(edited.managementAddress, freshFacts.proxyPort);
          restart = restartNotice(edited.managementAddress, {
            tls: tlsOn(answer),
            proxyPort: freshFacts.proxyPort,
            origin: window.location.origin,
          });
        }
        fresh = settingValuesOf(answer, freshFacts);
        if (editor.editor.versioned) {
          for (const id of SETTING_IDS.filter((id) => edited[id] !== base[id])) {
            if (editor.editor.draftMoved(saveCall(id, edited[id]).path, id === "managementAddress" ? addressSnapshot : snapshot)) moved.add(id);
          }
        }
      }
      let freshKeys: string[] | null = null;
      if (changes.length > 0) {
        const answer = await call<unknown>(API_KEYS);
        client.setQueryData([API_KEYS, "config-editor", editor.scope], answer);
        freshKeys = keysIn(answer);
        editor.editor.checkKeyTargets(changes, freshKeys);
      }
      return { edited, base, changes, fresh, freshKeys, restart, addressProblem, moved };
    },
    onSuccess: ({ edited, base, changes, fresh, freshKeys, restart, addressProblem, moved }) => {
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
        const settingsFound = settingChanges(base, edited, fresh).map((change) => ({ ...change,
          movedOnServer: editor.editor.versioned ? moved.has(change.id) : change.movedOnServer,
        }));
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
      // Review can stay open while another writer changes the file. Check
      // every field again before starting the batch, then guard each write.
      await editor.checkChanges(toSave.flatMap((pending) => pending.kind === "key"
        ? [] : [saveCall(pending.change.id, pending.change.after).path]));
      await editor.editor.checkKeyChanges(toSave.flatMap((pending) => pending.kind === "key" ? [pending.change] : []));
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
            setKeyChanges(editor.editor.versioned ? next : effectiveKeyChanges(savedKeys, next));
          }}
          readOnly={keysReadOnly}
          versioned={editor.editor.versioned}
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
            onDraft={(id) => {
              const snapshot = id === "managementAddress" ? facts.snapshot : config.snapshot;
              if (snapshot !== undefined) editor.editor.bindDraft(saveCall(id, settings.loaded[id]).path, snapshot);
            }}
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
          editor.editor.retainDrafts([]);
          setKeyChanges([]);
          setOutcome(null);
          read.reset();
        }}
        onReview={startReview}
        onReload={onReload}
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
        onReload={onReload}
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
  const editor = useConfigEditor();
  const config = useDraftConfigQuery<unknown>(editor, CONFIG);
  const factsQuery = useQuery({
    queryKey: [...FACTS, editor.scope],
    queryFn: async ({ signal }) => {
      const snapshot = await editor.editor.readValue<unknown>(V8_CONFIG, signal);
      return { data: await readServerFacts(() => Promise.resolve(snapshot.data)), snapshot };
    },
  });
  const facts = { ...factsQuery, data: factsQuery.data?.data, snapshot: factsQuery.data?.snapshot } as UseQueryResult<ServerFacts> & { snapshot: ConfigRead<unknown> | undefined };
  const keys = useConfigQuery<ApiKeysAnswer>(editor, API_KEYS);
  // Kept here, so they outlast the editor starting afresh below.
  const [keyChanges, setKeyChanges] = useState<KeyChange[]>([]);
  const [keysReadOnly, setKeysReadOnly] = useState(false);
  const [round, setRound] = useState(0);
  const reload = async () => {
    await editor.reload();
    setKeyChanges([]);
    onUnsavedChange(false);
    setRound((value) => value + 1);
  };
  return (
    // The form starts once, with the settings loaded, so it never counts
    // the values arriving as edits.
    <SettingsEditor
      key={
        config.data === undefined || (facts.dataUpdatedAt === 0 && facts.errorUpdatedAt === 0)
          ? "loading"
          : `loaded-${String(round)}`
      }
      editor={editor}
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
      onReload={() => { void reload().catch(() => { /* Query notices show failed reads; keep the draft intact. */ }); }}
    />
  );
}
