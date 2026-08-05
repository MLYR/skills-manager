import { useState } from "react";
import { useTranslation } from "react-i18next";
import { AlertTriangle, Loader2, X } from "lucide-react";
import type { AiAnalysisPreviewDto } from "../../lib/tauri";

interface Props {
  preview: AiAnalysisPreviewDto | null;
  busy: boolean;
  analysisCounts?: { parsed: number; unparsed: number } | null;
  onClose: () => void;
  onConfirm: (previewId: string) => Promise<void>;
}

/** Privacy and token preview before any analysis is enqueued. */
export function AiAnalysisPreviewDialog({ preview, busy, analysisCounts, onClose, onConfirm }: Props) {
  const { t } = useTranslation();
  const [expanded, setExpanded] = useState(false);
  if (!preview) return null;

  const handleConfirm = () => {
    void onConfirm(preview.preview_id);
  };

  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center">
      <div className="absolute inset-0 bg-black/70 backdrop-blur-sm" onClick={busy ? undefined : onClose} />
      <div className="relative bg-surface border border-border rounded-xl w-full max-w-lg p-5 shadow-2xl">
        <div className="flex items-center justify-between mb-4">
          <h2 className="text-[13px] font-semibold text-primary flex items-center gap-2">
            <AlertTriangle className="w-4 h-4 text-amber-400" />
            {t("ai.preview.title")}
          </h2>
          <button
            onClick={onClose}
            disabled={busy}
            className="text-muted hover:text-secondary p-1 rounded transition-colors outline-none disabled:opacity-40"
          >
            <X className="w-4 h-4" />
          </button>
        </div>

        <div className="space-y-3 text-[13px] leading-[18px] text-secondary">
          {analysisCounts ? (
            <p className="rounded-lg border border-border-subtle bg-surface/70 px-3 py-2 text-[12px] text-muted">
              {t("ai.preview.analysisCounts", analysisCounts)}
            </p>
          ) : null}
          <p>{t("ai.preview.scope", {
            total: preview.total_targets,
            valid: preview.valid_documents,
            missing: preview.missing_documents,
            unreadable: preview.unreadable_documents,
            skipped: preview.skipped_targets,
          })}</p>
          <p>{t("ai.preview.usage", {
            characters: preview.total_characters,
            input: preview.estimated_input_tokens,
            output: preview.estimated_output_tokens,
          })}</p>
          <p className="text-muted">{t("ai.preview.tokenNote")}</p>
          <button
            type="button"
            onClick={() => setExpanded((prev) => !prev)}
            className="text-[12px] font-medium text-accent hover:underline"
          >
            {expanded ? t("ai.preview.collapseContent") : t("ai.preview.expandContent")}
          </button>
          {expanded && (
            <div className="max-h-64 space-y-2 overflow-y-auto rounded-lg border border-border-subtle bg-surface/70 p-3">
              {preview.items.map((item, index) => (
                <div key={`${item.target.kind}-${index}`} className="space-y-1">
                  <div className="flex items-center gap-2">
                    <span className="font-mono text-[12px] text-primary">{item.skill_name}</span>
                    <span className="text-[11px] text-muted">{item.character_count} chars</span>
                  </div>
                  <pre className="whitespace-pre-wrap rounded bg-bg-secondary p-2 text-[11.5px] leading-[16px] text-muted">
                    {item.content || t("ai.preview.noContent")}
                  </pre>
                </div>
              ))}
            </div>
          )}
          {busy && (
            <div className="flex items-center gap-2 text-muted">
              <Loader2 className="h-3.5 w-3.5 animate-spin" />
              <span>{t("ai.preview.duringConfirmation")}</span>
            </div>
          )}
        </div>

        <div className="mt-5 flex justify-end gap-2">
          <button
            onClick={onClose}
            disabled={busy}
            className="px-3 py-1.5 rounded-[4px] text-[13px] font-medium text-tertiary hover:text-secondary hover:bg-surface-hover transition-colors outline-none disabled:opacity-40"
          >
            {t("common.cancel")}
          </button>
          <button
            onClick={handleConfirm}
            disabled={busy}
            className="px-3 py-1.5 rounded-[4px] bg-accent-dark hover:bg-accent text-white text-[13px] font-medium transition-colors disabled:opacity-50 disabled:cursor-not-allowed border border-accent-border outline-none"
          >
            {busy ? t("ai.preview.enqueuing") : t("ai.preview.confirm")}
          </button>
        </div>
      </div>
    </div>
  );
}
