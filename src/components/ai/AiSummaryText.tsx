import { useTranslation } from "react-i18next";
import { cn } from "../../utils";

interface Props {
  oneLine: string | null;
  stale: boolean;
  className?: string;
}

/** List second-column summary: stale results stay visible with a marker. */
export function AiSummaryText({ oneLine, stale, className }: Props) {
  const { t } = useTranslation();
  return (
    <span className={cn("inline-flex min-w-0 items-center gap-1.5", className)}>
      {stale && (
        <span className="shrink-0 rounded-full bg-amber-500/12 px-1.5 py-0.5 text-[10.5px] font-medium text-amber-600 dark:text-amber-400">
          {t("ai.list.stale")}
        </span>
      )}
      <span className="truncate">{oneLine || "—"}</span>
    </span>
  );
}
