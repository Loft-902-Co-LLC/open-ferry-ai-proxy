import { CircleAlert, CircleCheck, Info, TriangleAlert, type LucideIcon } from "lucide-react";
import type { ReactNode } from "react";

import { cn } from "../lib/cn";

export type AlertTone = "info" | "ok" | "warn" | "danger";

const TONES: Record<AlertTone, { box: string; icon: string; Icon: LucideIcon }> = {
  info: { box: "border-accent/50 bg-accent-soft", icon: "text-accent", Icon: Info },
  ok: { box: "border-ok/50 bg-ok-soft", icon: "text-ok", Icon: CircleCheck },
  warn: { box: "border-warn/60 bg-warn-soft", icon: "text-warn", Icon: TriangleAlert },
  danger: { box: "border-danger/50 bg-danger-soft", icon: "text-danger", Icon: CircleAlert },
};

export interface AlertProps {
  tone?: AlertTone;
  title?: ReactNode;
  children?: ReactNode;
  /** Announce it as it appears: for errors that follow an action. */
  live?: boolean;
  className?: string;
}

export function Alert({ tone = "info", title, children, live = false, className }: AlertProps) {
  const { box, icon, Icon } = TONES[tone];
  return (
    <div
      role={live ? (tone === "danger" ? "alert" : "status") : undefined}
      className={cn("flex gap-3 rounded-md border px-3.5 py-3", box, className)}
    >
      <Icon aria-hidden="true" className={cn("mt-0.5 size-4 shrink-0", icon)} />
      <div className="min-w-0 space-y-1">
        {title !== undefined && <p className="font-semibold">{title}</p>}
        {children !== undefined && <div className="space-y-2 text-fg">{children}</div>}
      </div>
    </div>
  );
}
