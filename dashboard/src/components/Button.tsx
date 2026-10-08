import type { ButtonHTMLAttributes, Ref } from "react";

import { cn } from "../lib/cn";

export type ButtonVariant = "primary" | "secondary" | "ghost" | "danger";
export type ButtonSize = "sm" | "md";

const VARIANTS: Record<ButtonVariant, string> = {
  primary: "border-transparent bg-accent-strong text-accent-fg hover:bg-accent-strong/90",
  secondary: "border-line bg-surface text-fg hover:bg-raised",
  ghost: "border-transparent text-fg hover:bg-raised",
  danger: "border-transparent bg-danger-strong text-danger-fg hover:bg-danger-strong/90",
};

/** A small button is still 44 px tall where the pointer is a finger. */
const SIZES: Record<ButtonSize, string> = {
  sm: "h-8 px-2.5 text-sm pointer-coarse:min-h-11",
  md: "h-9 px-3.5 text-sm",
};

/** The classes of a button, for links styled as one. */
export function buttonClasses(variant: ButtonVariant = "secondary", size: ButtonSize = "md") {
  return cn(
    "inline-flex shrink-0 items-center justify-center gap-2 rounded-md border font-medium whitespace-nowrap",
    "no-underline transition-colors hover:no-underline disabled:cursor-not-allowed disabled:opacity-60",
    VARIANTS[variant],
    SIZES[size],
  );
}

export interface ButtonProps extends ButtonHTMLAttributes<HTMLButtonElement> {
  variant?: ButtonVariant;
  size?: ButtonSize;
  ref?: Ref<HTMLButtonElement>;
}

export function Button({
  variant = "secondary",
  size = "md",
  className,
  type = "button",
  ...props
}: ButtonProps) {
  return <button type={type} className={cn(buttonClasses(variant, size), className)} {...props} />;
}
