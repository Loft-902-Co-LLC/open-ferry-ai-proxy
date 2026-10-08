import { useMutation, useQueryClient } from "@tanstack/react-query";
import { Upload } from "lucide-react";
import type { ChangeEvent } from "react";

import { callProblem } from "../../api/access";
import { AUTH_FILES, type UploadAnswer } from "../../api/credentials";
import { useApiCall } from "../../api/hooks";
import { Alert } from "../../components/Alert";
import { buttonClasses, type ButtonSize, type ButtonVariant } from "../../components/Button";
import { ProblemNotice } from "../../components/ProblemNotice";
import { Spinner } from "../../components/Spinner";
import { cn } from "../../lib/cn";
import { formatInteger } from "../../lib/format";

/** What an upload did, in words. */
export function UploadResult({ answer, files }: { answer: UploadAnswer; files: File[] }) {
  const failed = answer.failed ?? [];
  const uploaded = answer.uploaded ?? (answer.status === "ok" ? files.length : 0);
  if (answer.status === "ok" && failed.length === 0) {
    return (
      <Alert tone="ok" live title={uploaded === 1 ? "Uploaded" : `Uploaded ${formatInteger(uploaded)} files`}>
        <p>
          {(answer.files ?? files.map((file) => file.name)).join(", ")}. The server uses{" "}
          {uploaded === 1 ? "it" : "them"} from now on.
        </p>
      </Alert>
    );
  }
  return (
    <Alert
      tone="warn"
      live
      title={`Uploaded ${formatInteger(uploaded)} of ${formatInteger(uploaded + failed.length)} files`}
    >
      <p>These weren&apos;t uploaded:</p>
      <ul className="list-disc pl-5">
        {failed.map((failure) => (
          <li key={failure.name}>
            <span className="font-medium break-all">{failure.name}</span>: {failure.error}
          </li>
        ))}
      </ul>
    </Alert>
  );
}

export interface UploadLook {
  variant?: ButtonVariant;
  size?: ButtonSize;
}

/**
 * Uploading credential files, as the server's own sign-ins save them: the
 * button that chooses them, and what the upload did.
 */
export function useUpload({ variant = "secondary", size = "sm" }: UploadLook = {}) {
  const call = useApiCall();
  const client = useQueryClient();
  const upload = useMutation({
    mutationFn: (files: File[]) => {
      const form = new FormData();
      for (const file of files) {
        form.append("file", file, file.name);
      }
      return call<UploadAnswer>(AUTH_FILES, { method: "POST", body: form });
    },
    onSettled: () => client.invalidateQueries({ queryKey: [AUTH_FILES] }),
  });

  const choose = (event: ChangeEvent<HTMLInputElement>) => {
    const files = [...(event.target.files ?? [])];
    // Clear it, so choosing the same file again uploads it again.
    event.target.value = "";
    if (files.length > 0) {
      upload.mutate(files);
    }
  };

  return {
    button: (
      <label
        className={cn(
          buttonClasses(variant, size),
          "cursor-pointer has-[:focus-visible]:outline-2 has-[:focus-visible]:outline-offset-2 has-[:focus-visible]:outline-accent",
          upload.isPending && "cursor-wait opacity-60",
        )}
      >
        {upload.isPending ? <Spinner /> : <Upload aria-hidden="true" className="size-4" />}
        Upload credential files
        <input
          type="file"
          multiple
          accept=".json,application/json"
          className="sr-only"
          disabled={upload.isPending}
          onChange={choose}
        />
      </label>
    ),
    result: upload.isSuccess ? (
      <UploadResult answer={upload.data} files={upload.variables} />
    ) : upload.isError ? (
      <ProblemNotice problem={callProblem(upload.error)} live />
    ) : null,
  };
}
