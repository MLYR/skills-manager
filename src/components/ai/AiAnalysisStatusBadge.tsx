import { Sparkles } from "lucide-react";
import { useTranslation } from "react-i18next";
import { cn } from "../../utils";
import type { AiAnalysisStatus } from "../../lib/tauri";

interface Props {
  status: AiAnalysisStatus;
  className?: string;
}

const STATUS_LABELS: Record<AiAnalysisStatus, string> = {
  unconfigured: "unconfigured",
  unparsed: "unparsed",
  queued: "queued",
  running: "running",
  paused: "paused",
  failed: "failed",
  succeeded: "succeeded",
  stale: "stale",
  no_document: "noDocument",
  unreadable: "unreadable",
};

const STATUS_STYLES: Record<AiAnalysisStatus, string> = {
  unconfigured: "bg-surface-hover text-muted",
  unparsed: "bg-surface-hover text-muted",
  queued: "bg-sky-500/10 text-sky-700 dark:text-sky-300",
  running: "bg-sky-500/10 text-sky-700 dark:text-sky-300",
  paused: "bg-surface-hover text-muted",
  failed: "bg-red-500/10 text-red-600 dark:text-red-400",
  succeeded: "bg-emerald-500/10 text-emerald-600 dark:text-emerald-400",
  stale: "bg-amber-500/12 text-amber-700 dark:text-amber-300",
  no_document: "bg-surface-hover text-muted",
  unreadable: "bg-red-500/10 text-red-600 dark:text-red-400",
};

/** Compact, shared status indicator keeps AI state distinct from sync state. */
export function AiAnalysisStatusBadge({ status, className }: Props) {
  const { t } = useTranslation();
  const label = t(`ai.status.${STATUS_LABELS[status]}`);

  return (
    <span
      title={`${t("ai.tab")} · ${label}`}
      className={cn(
        "inline-flex shrink-0 items-center gap-1 rounded-full px-1.5 py-0.5 text-[10.5px] font-medium",
        STATUS_STYLES[status],
        className,
      )}
    >
      <Sparkles className="h-3 w-3" />
      <span>AI · {label}</span>
    </span>
  );
}
