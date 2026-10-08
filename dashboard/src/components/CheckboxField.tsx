import { Check } from "lucide-react";
import { useId, type InputHTMLAttributes, type ReactNode, type Ref } from "react";

import { cn } from "../lib/cn";

export interface CheckboxProps extends Omit<InputHTMLAttributes<HTMLInputElement>, "type"> {
  ref?: Ref<HTMLInputElement>;
}

/**
 * A native checkbox, drawn so that its edge keeps 3:1 against the page in
 * both themes. `className` goes on the box's wrapper, for placing it. With
 * forced colours, the system draws it.
 */
export function Checkbox({ className, ref, ...input }: CheckboxProps) {
  return (
    <span className={cn("relative inline-flex size-4 shrink-0 has-disabled:opacity-60", className)}>
      <input
        ref={ref}
        type="checkbox"
        className={cn(
          "peer m-0 size-4 appearance-none rounded-[3px] border border-control bg-surface",
          "checked:border-accent-strong checked:bg-accent-strong forced-colors:appearance-auto",
        )}
        {...input}
      />
      <Check
        aria-hidden="true"
        strokeWidth={3}
        className={cn(
          "pointer-events-none absolute inset-0 m-auto size-3 text-accent-fg opacity-0",
          "peer-checked:opacity-100 forced-colors:hidden",
        )}
      />
    </span>
  );
}

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
      <Checkbox
        id={inputId}
        ref={ref}
        aria-describedby={hint === undefined ? undefined : hintId}
        className="mt-1"
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
