import { useCallback, useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { Copy, Loader2, RefreshCw } from "lucide-react";
import { toast } from "sonner";
import {
  enqueueAiAnalysis,
  getAiAnalysis,
  previewAiAnalysis,
  type AiAnalysisDetailDto,
  type AiAnalysisPreviewDto,
  type AiTargetRef,
} from "../../lib/tauri";
import { isAiCommandError, getErrorMessage } from "../../lib/error";
import { AiAnalysisPreviewDialog } from "./AiAnalysisPreviewDialog";

interface Props {
  target: AiTargetRef;
}

/** Detail-page AI tab: structured result, statuses, disclaimer, re-analyze. */
export function AiAnalysisPanel({ target }: Props) {
  const { t } = useTranslation();
  const [detail, setDetail] = useState<AiAnalysisDetailDto | null>(null);
  const [loading, setLoading] = useState(true);
  const [preview, setPreview] = useState<AiAnalysisPreviewDto | null>(null);
  const [previewBusy, setPreviewBusy] = useState(false);
  const [refreshing, setRefreshing] = useState(false);
  const requestRef = useRef(0);

  const load = useCallback(() => {
    requestRef.current += 1;
    const requestId = requestRef.current;
    setLoading(true);
    getAiAnalysis({ target })
      .then((next) => {
        if (requestId === requestRef.current) setDetail(next);
      })
      .catch(() => {
        if (requestId === requestRef.current) setDetail(null);
      })
      .finally(() => {
        if (requestId === requestRef.current) setLoading(false);
      });
  }, [target]);

  useEffect(() => {
    load();
  }, [load]);

  const startPreview = async () => {
    try {
      const next = await previewAiAnalysis({ targets: [target], mode: "force" });
      setPreview(next);
    } catch (error) {
      toast.error(aiErrorMessage(error));
    }
  };

  const confirmEnqueue = async (previewId: string) => {
    setPreviewBusy(true);
    try {
      await enqueueAiAnalysis({ preview_id: previewId });
      setPreview(null);
      toast.success(t("ai.panel.enqueued"));
      setRefreshing(true);
      load();
    } catch (error) {
      toast.error(aiErrorMessage(error));
    } finally {
      setPreviewBusy(false);
      setRefreshing(false);
    }
  };

  const copyPrompt = (prompt: string) => {
    void navigator.clipboard.writeText(prompt).then(() => toast.success(t("ai.panel.copied")));
  };

  if (loading && !detail) {
    return <div className="mt-12 text-center text-[13px] text-muted">{t("common.loading")}</div>;
  }

  const status = detail?.status ?? "unparsed";
  const result = detail?.result ?? null;

  return (
    <div className="space-y-4">
      <div className="flex flex-wrap items-center gap-2">
        <span className="inline-flex items-center gap-1.5 rounded-full bg-surface-hover px-2.5 py-1 text-[12px] font-medium text-secondary">
          {statusLabel(t, status)}
        </span>
        {detail?.active_job && (
          <span className="inline-flex items-center gap-1.5 text-[12px] text-muted">
            <Loader2 className="h-3 w-3 animate-spin" />
            {t("ai.panel.jobAttempt", {
              attempt: detail.active_job.attempt_count,
              total: 3,
            })}
          </span>
        )}
        <button
          type="button"
          onClick={startPreview}
          disabled={refreshing}
          className="inline-flex items-center gap-1.5 rounded-full bg-surface-hover px-2.5 py-1 text-[12px] font-medium text-muted transition-colors hover:text-secondary disabled:opacity-50"
        >
          <RefreshCw className="h-3 w-3" />
          {t("ai.panel.reanalyze")}
        </button>
      </div>

      {detail?.last_error && (
        <div className="rounded-lg border border-red-500/25 bg-red-500/8 px-3 py-2 text-[12.5px] text-red-600 dark:text-red-400">
          {detail.last_error.message}
        </div>
      )}

      {status === "no_document" && (
        <div className="rounded-lg border border-border-subtle bg-surface/70 px-3 py-2 text-[12.5px] text-muted">
          {t("ai.panel.noDocument")}
        </div>
      )}
      {status === "unconfigured" && (
        <div className="rounded-lg border border-border-subtle bg-surface/70 px-3 py-2 text-[12.5px] text-muted">
          {t("ai.panel.unconfigured")}
        </div>
      )}
      {status === "unparsed" && (
        <div className="rounded-lg border border-border-subtle bg-surface/70 px-3 py-2 text-[12.5px] text-muted">
          {t("ai.panel.unparsed")}
        </div>
      )}
      {status === "stale" && (
        <div className="rounded-lg border border-amber-500/25 bg-amber-500/8 px-3 py-2 text-[12.5px] text-amber-600 dark:text-amber-400">
          {t("ai.panel.stale")}
        </div>
      )}

      {result && (
        <>
          <div className="text-[15px] font-semibold leading-[22px] text-primary">{result.one_line}</div>
          <Section title={t("ai.panel.whatItDoes")} content={result.what_it_does} />
          <ListSection title={t("ai.panel.whenToUse")} items={result.when_to_use} />
          <ListSection title={t("ai.panel.howToUse")} items={result.how_to_use} />
          <ListSection title={t("ai.panel.requirements")} items={result.requirements} />
          <ListSection title={t("ai.panel.notFor")} items={result.not_for} />
          <ListSection title={t("ai.panel.warnings")} items={result.warnings} />
          {result.example_prompts.length > 0 && (
            <div>
              <div className="mb-1.5 text-[12px] font-medium uppercase tracking-[0.08em] text-faint">
                {t("ai.panel.examplePrompts")}
              </div>
              <div className="space-y-2">
                {result.example_prompts.map((prompt, index) => (
                  <div
                    key={index}
                    className="group flex items-start gap-2 rounded-lg border border-border-subtle bg-surface/70 p-2.5"
                  >
                    <code className="min-w-0 flex-1 whitespace-pre-wrap text-[12px] leading-[17px] text-secondary">
                      {prompt}
                    </code>
                    <button
                      type="button"
                      onClick={() => copyPrompt(prompt)}
                      className="shrink-0 text-muted transition-colors hover:text-secondary"
                      title={t("ai.panel.copy")}
                    >
                      <Copy className="h-3.5 w-3.5" />
                    </button>
                  </div>
                ))}
              </div>
            </div>
          )}
          <p className="text-[11.5px] text-faint">{t("ai.panel.disclaimer")}</p>
        </>
      )}

      <AiAnalysisPreviewDialog
        preview={preview}
        busy={previewBusy}
        onClose={() => setPreview(null)}
        onConfirm={confirmEnqueue}
      />
    </div>
  );
}

function Section({ title, content }: { title: string; content: string }) {
  return (
    <div>
      <div className="mb-1.5 text-[12px] font-medium uppercase tracking-[0.08em] text-faint">
        {title}
      </div>
      <p className="text-[13px] leading-[19px] text-secondary">{content}</p>
    </div>
  );
}

function ListSection({ title, items }: { title: string; items: string[] }) {
  if (items.length === 0) return null;
  return (
    <div>
      <div className="mb-1.5 text-[12px] font-medium uppercase tracking-[0.08em] text-faint">
        {title}
      </div>
      <ul className="list-disc space-y-1 pl-4 text-[13px] leading-[19px] text-secondary">
        {items.map((item, index) => (
          <li key={index}>{item}</li>
        ))}
      </ul>
    </div>
  );
}

function statusLabel(t: (key: string) => string, status: string) {
  switch (status) {
    case "succeeded":
      return t("ai.status.succeeded");
    case "stale":
      return t("ai.status.stale");
    case "failed":
      return t("ai.status.failed");
    case "queued":
      return t("ai.status.queued");
    case "running":
      return t("ai.status.running");
    case "paused":
      return t("ai.status.paused");
    case "no_document":
      return t("ai.status.noDocument");
    case "unreadable":
      return t("ai.status.unreadable");
    case "unconfigured":
      return t("ai.status.unconfigured");
    default:
      return t("ai.status.unparsed");
  }
}

function aiErrorMessage(error: unknown) {
  if (isAiCommandError(error)) return error.message;
  return getErrorMessage(error, "");
}
