import { render, screen, within } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";

import { ApiError } from "../../api/client";
import { ClientKeysCard, type ClientKeysCardProps } from "./ClientKeysCard";

describe.each([false, true])("client key config guidance (versioned: %s)", (versioned) => {
  it.each(["writable", "unsupported route", "read-only"])(
    "names both config layouts when %s",
    (state) => {
      const unsupported = state === "unsupported route";
      const keys = {
        data: unsupported ? undefined : { "api-keys": ["sk-test-client-config-path-0001"] },
        error: unsupported ? new ApiError(404, null, null, null) : null,
      } as ClientKeysCardProps["keys"];

      render(
        <ClientKeysCard
          keys={keys}
          changes={[]}
          onChanges={vi.fn()}
          readOnly={state === "read-only"}
          versioned={versioned}
        />,
      );

      const card = within(screen.getByRole("region", { name: "Client API keys" }));
      const description = card.getByText(/The keys clients send to use the proxy/);
      expect(description).toBeVisible();
      expect(description).toHaveTextContent(
        "kept in config.yaml as access.api-keys in the current layout.",
      );
      expect(description).toHaveTextContent("Older files use a top-level api-keys list.");
      expect(description).toHaveTextContent(
        "The current layout's top-level api-keys mapping holds provider keys.",
      );

      if (state === "writable") {
        expect(card.getByRole("button", { name: "Add a client key" })).toBeVisible();
      } else {
        const guidance = card.getByText(/Add and delete them under/);
        expect(guidance).toBeVisible();
        expect(guidance).toHaveTextContent(
          "Add and delete them under access.api-keys in the current config.yaml layout (the top-level api-keys list in older files)",
        );
        expect(card.queryByRole("button", { name: "Add a client key" })).not.toBeInTheDocument();
      }
    },
  );
});
