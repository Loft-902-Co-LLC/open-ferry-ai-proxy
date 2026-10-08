import { Eye, EyeOff, TriangleAlert } from "lucide-react";
import { useId, useState, type InputHTMLAttributes, type ReactNode, type Ref } from "react";

import { cn } from "../lib/cn";

/** A text input's classes, for an input or textarea outside this field. Its
 * border is `border-control`, which keeps the 3:1 a control's edge needs. */
export const inputClasses =
  "h-9 w-full min-w-0 rounded-md border border-control bg-surface px-3 text-fg placeholder:text-muted " +
  "aria-invalid:border-danger disabled:opacity-60";

export interface TextFieldProps extends Omit<InputHTMLAttributes<HTMLInputElement>, "children"> {
  label: ReactNode;
  /** Help shown under the field, and read with it. */
  hint?: ReactNode;
  /** What is wrong with the value, if anything. */
  error?: string | undefined;
  /**
   * A problem with a value that doesn't stop anything, such as one the user
   * hasn't changed: shown, without marking the field invalid.
   */
  warning?: string | undefined;
  /** A secret: masked, with a button to show it. */
  secret?: boolean;
  /** The show button's name, such as "Show key". */
  revealLabel?: string;
  ref?: Ref<HTMLInputElement>;
}

/** A labelled text input, with its hint, warning and error tied to it. */
export function TextField({
  label,
  hint,
  error,
  warning,
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
  const warningId = `${inputId}-warning`;
  const errorId = `${inputId}-error`;
  const [shown, setShown] = useState(false);
  const describedBy =
    [
      hint === undefined ? null : hintId,
      warning === undefined ? null : warningId,
      error === undefined ? null : errorId,
    ]
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
      {warning !== undefined && (
        <p id={warningId} className="flex items-start gap-1.5">
          <TriangleAlert aria-hidden="true" className="mt-0.5 size-4 shrink-0 text-warn" />
          <span>{warning}</span>
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
