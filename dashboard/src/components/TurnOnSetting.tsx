import { useMutation, useQueryClient, type QueryKey } from "@tanstack/react-query";
import { Power } from "lucide-react";

import { callProblem, cantSaveConfig } from "../api/access";
import { useApiCall } from "../api/hooks";
import { Alert } from "./Alert";
import { Button } from "./Button";
import { Code } from "./Code";
import { ProblemNotice } from "./ProblemNotice";
import { Spinner } from "./Spinner";

export interface TurnOnSettingProps {
  /** The management route of a boolean setting, which takes `{"value": true}`. */
  path: string;
  /** The button's text, such as "Start recording". */
  label: string;
  /** The setting's key in config.yaml, for a server that can't save it. */
  configKey: string;
  /** The queries that change with it. */
  invalidate: readonly QueryKey[];
}

/**
 * A button that turns a boolean setting on through the management API,
 * which saves it to config.yaml and answers once the server uses it. A
 * server that can't save the file (without the route, or without a way to
 * save it) is told apart, with the line to put in config.yaml instead.
 */
export function TurnOnSetting({ path, label, configKey, invalidate }: TurnOnSettingProps) {
  const call = useApiCall();
  const client = useQueryClient();
  const turnOn = useMutation({
    mutationFn: () => call(path, { method: "PUT", json: { value: true } }),
    onSuccess: () =>
      Promise.all(invalidate.map((queryKey) => client.invalidateQueries({ queryKey }))),
  });
  return (
    <div className="space-y-2">
      <Button
        size="sm"
        variant="primary"
        disabled={turnOn.isPending}
        onClick={() => {
          turnOn.mutate();
        }}
      >
        {turnOn.isPending ? <Spinner /> : <Power aria-hidden="true" className="size-4" />}
        {label}
      </Button>
      {turnOn.isError &&
        (cantSaveConfig(turnOn.error) ? (
          <Alert tone="warn" live title="This server can't save config.yaml">
            <p>
              Nothing was changed. To turn it on, set <Code>{configKey}: true</Code> in config.yaml
              itself; the server picks it up when it reloads the file.
            </p>
          </Alert>
        ) : (
          <ProblemNotice problem={callProblem(turnOn.error)} live />
        ))}
    </div>
  );
}
