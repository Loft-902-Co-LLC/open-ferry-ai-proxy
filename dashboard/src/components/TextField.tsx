import { Eye, EyeOff } from "lucide-react";
import { useId, useState, type InputHTMLAttributes, type ReactNode, type Ref } from "react";

import { cn } from "../lib/cn";

export const inputClasses =
  "h-9 w-full min-w-0 rounded-md border border-line bg-surface px-3 text-fg placeholder:text-muted " +
  "aria-invalid:border-danger disabled:opacity-60";

export interface TextFieldProps extends Omit<InputHTMLAttributes<HTMLInputElement>, "children"> {
  label: ReactNode;
  /** Help shown under the field, and read with it. */
  hint?: ReactNode;
  /** What is wrong with the value, if anything. */
  error?: string | undefined;
  /** A secret: masked, with a button to show it. */
  secret?: boolean;
  /** The show button's name, such as "Show key". */
  revealLabel?: string;
  ref?: Ref<HTMLInputElement>;
}

/** A labelled text input, with its hint and error tied to it. */
export function TextField({
  label,
  hint,
  error,
  secret = false,
  revealLabel = "Show value",
  className,
  id,
  type,
  ref,
  ...input
}: TextFieldProps) {
  const generatedId = useId();
  const inputId = id ?? generatedId;
  const hintId = `${inputId}-hint`;
  const errorId = `${inputId}-error`;
  const [shown, setShown] = useState(false);
  const describedBy =
    [hint === undefined ? null : hintId, error === undefined ? null : errorId]
      .filter((value) => value !== null)
      .join(" ") || undefined;

  return (
    <div className={cn("space-y-1.5", className)}>
      <label htmlFor={inputId} className="block font-medium">
        {label}
      </label>
      <div className="relative">
        <input
          id={inputId}
          ref={ref}
          type={secret ? (shown ? "text" : "password") : type}
          aria-invalid={error === undefined ? undefined : true}
          aria-describedby={describedBy}
          className={cn(inputClasses, secret && "pr-10 font-mono")}
          {...(secret ? { autoComplete: "off", spellCheck: false, autoCapitalize: "none" } : {})}
          {...input}
        />
        {secret && (
          <button
            type="button"
            aria-label={revealLabel}
            aria-pressed={shown}
            title={revealLabel}
            onClick={() => {
              setShown((value) => !value);
            }}
            className="absolute inset-y-0 right-0 flex w-9 items-center justify-center rounded-r-md text-muted hover:text-fg"
          >
            {shown ? (
              <EyeOff aria-hidden="true" className="size-4" />
            ) : (
              <Eye aria-hidden="true" className="size-4" />
            )}
          </button>
        )}
      </div>
      {hint !== undefined && (
        <p id={hintId} className="text-muted">
          {hint}
        </p>
      )}
      {error !== undefined && (
        <p id={errorId} className="font-medium text-danger">
          {error}
        </p>
      )}
    </div>
  );
}
