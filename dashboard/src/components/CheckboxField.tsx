import { useId, type InputHTMLAttributes, type ReactNode, type Ref } from "react";

import { cn } from "../lib/cn";

export interface CheckboxFieldProps
  extends Omit<InputHTMLAttributes<HTMLInputElement>, "children" | "type"> {
  label: ReactNode;
  /** Help shown under the label, and read with it. */
  hint?: ReactNode;
  ref?: Ref<HTMLInputElement>;
}

/** A labelled native checkbox, with its hint tied to it. */
export function CheckboxField({ label, hint, className, id, ref, ...input }: CheckboxFieldProps) {
  const generatedId = useId();
  const inputId = id ?? generatedId;
  const hintId = `${inputId}-hint`;
  return (
    <div className={cn("flex items-start gap-3", className)}>
      <input
        id={inputId}
        ref={ref}
        type="checkbox"
        aria-describedby={hint === undefined ? undefined : hintId}
        className="mt-1 size-4 shrink-0 accent-accent disabled:opacity-60"
        {...input}
      />
      <div className="min-w-0 space-y-0.5">
        <label htmlFor={inputId} className="block font-medium">
          {label}
        </label>
        {hint !== undefined && (
          <p id={hintId} className="text-muted">
            {hint}
          </p>
        )}
      </div>
    </div>
  );
}
