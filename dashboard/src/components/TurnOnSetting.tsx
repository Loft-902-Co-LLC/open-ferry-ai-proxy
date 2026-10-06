import { useMutation, useQueryClient, type QueryKey } from "@tanstack/react-query";
import { Power } from "lucide-react";

import { callProblem } from "../api/access";
import { isUnsupportedRoute } from "../api/client";
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
  /** The setting's key in config.yaml, for when it can't be changed here. */
  configKey: string;
  /** The queries that change with it. */
  invalidate: readonly QueryKey[];
}

/**
 * A button that turns a boolean setting on through the management API. A
 * server without that route is told apart: the setting is then changed in
 * config.yaml.
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
        (isUnsupportedRoute(turnOn.error) ? (
          <Alert tone="info" live title="This server can't change settings yet">
            <p>
              Set <Code>{configKey}: true</Code> in config.yaml instead.
            </p>
          </Alert>
        ) : (
          <ProblemNotice problem={callProblem(turnOn.error)} live />
        ))}
    </div>
  );
}
