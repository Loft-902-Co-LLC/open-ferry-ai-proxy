import { useId, type ReactNode, type Ref, type SelectHTMLAttributes } from "react";

import { cn } from "../lib/cn";

export const selectClasses =
  "h-9 min-w-0 rounded-md border border-control bg-surface pr-8 pl-2.5 text-fg aria-invalid:border-danger disabled:opacity-60";

export interface SelectOption {
  value: string;
  label: string;
}

export interface SelectFieldProps extends Omit<SelectHTMLAttributes<HTMLSelectElement>, "children"> {
  label: ReactNode;
  options: readonly SelectOption[];
  hint?: ReactNode;
  error?: string | undefined;
  /** Show the label beside the select, not above it. */
  inline?: boolean;
  ref?: Ref<HTMLSelectElement>;
}

/**
 * A labelled native select. Native, not a custom listbox: it works with
 * every input method and needs no injected styles.
 */
export function SelectField({
  label,
  options,
  hint,
  error,
  inline = false,
  className,
  id,
  ref,
  ...select
}: SelectFieldProps) {
  const generatedId = useId();
  const selectId = id ?? generatedId;
  const hintId = `${selectId}-hint`;
  const errorId = `${selectId}-error`;
  const describedBy =
    [hint === undefined ? null : hintId, error === undefined ? null : errorId]
      .filter((value) => value !== null)
      .join(" ") || undefined;
  return (
    <div className={cn(inline ? "flex items-center gap-2" : "space-y-1.5", className)}>
      <label htmlFor={selectId} className={cn("font-medium", inline ? "text-muted" : "block")}>
        {label}
      </label>
      <select
        id={selectId}
        ref={ref}
        aria-invalid={error === undefined ? undefined : true}
        aria-describedby={describedBy}
        className={cn(selectClasses, !inline && "w-full")}
        {...select}
      >
        {options.map((option) => (
          <option key={option.value} value={option.value}>
            {option.label}
          </option>
        ))}
      </select>
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
