import { useCallback, useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { toast } from "sonner";
import { Pause, Play, XCircle, RotateCcw, Plus } from "lucide-react";
import { useApp } from "../context/AppContext";
import {
  cancelAiAnalysisBatch,
  cancelAiAnalysisJob,
  enqueueAiAnalysis,
  getAiAnalysisQueueStats,
  listAiAnalysisBatches,
  listAiAnalysisJobs,
  pauseAiAnalysisBatch,
  previewAiAnalysis,
  resumeAiAnalysisBatch,
  retryAiAnalysisJob,
  type AiAnalysisPreviewDto,
  type AiBatchDto,
  type AiJobDto,
  type AiQueueStatsDto,
  type AiTargetRef,
} from "../lib/tauri";
import { isAiCommandError, getErrorMessage } from "../lib/error";
import { AiAnalysisPreviewDialog } from "../components/ai/AiAnalysisPreviewDialog";

export function AiAnalysisManager() {
  const { t } = useTranslation();
  const { managedSkills } = useApp();
  const [stats, setStats] = useState<AiQueueStatsDto | null>(null);
  const [batches, setBatches] = useState<AiBatchDto[]>([]);
  const [selectedBatchId, setSelectedBatchId] = useState<string | null>(null);
  const [jobs, setJobs] = useState<AiJobDto[]>([]);
  const [loading, setLoading] = useState(true);
  const [preview, setPreview] = useState<AiAnalysisPreviewDto | null>(null);
  const [previewBusy, setPreviewBusy] = useState(false);
  const [retryJob, setRetryJob] = useState<AiJobDto | null>(null);
  const requestRef = useRef(0);

  const loadBatches = useCallback(() => {
    listAiAnalysisBatches({ limit: 100 })
      .then((nextBatches) => {
        setBatches(nextBatches.items);
        setSelectedBatchId((current) => {
          if (current && nextBatches.items.some((batch) => batch.id === current)) return current;
          return nextBatches.items[0]?.id ?? null;
        });
      })
      .catch((error) => toast.error(aiErrorMessage(error)));
  }, []);

  const refresh = useCallback(() => {
    requestRef.current += 1;
    const requestId = requestRef.current;
    getAiAnalysisQueueStats()
      .then((nextStats) => {
        if (requestId !== requestRef.current) return;
        setStats(nextStats);
      })
      .catch((error) => {
        if (requestId === requestRef.current) toast.error(aiErrorMessage(error));
      })
      .finally(() => {
        if (requestId === requestRef.current) setLoading(false);
    });
    loadBatches();
  }, [loadBatches]);

  useEffect(() => {
    refresh();
    // Real backend state only: the fast interval re-reads batches/jobs, never
    // the full filesystem-backed stats scan, and never fabricates progress.
    const timer = window.setInterval(loadBatches, 2_000);
    return () => window.clearInterval(timer);
  }, [refresh, loadBatches]);

  useEffect(() => {
    if (!selectedBatchId) {
      setJobs([]);
      return;
    }
    listAiAnalysisJobs({ batch_id: selectedBatchId, limit: 100 })
      .then((page) => setJobs(page.items))
      .catch((error) => toast.error(aiErrorMessage(error)));
  }, [selectedBatchId, batches]);

  const startBatchPreview = async () => {
    const targets: AiTargetRef[] = managedSkills.map((skill) => ({
      kind: "managed",
      skill_id: skill.id,
    }));
    if (targets.length === 0) {
      toast.error(t("ai.manager.noSkills"));
      return;
    }
    try {
      const next = await previewAiAnalysis({ targets, mode: "missing_or_stale" });
      setPreview(next);
    } catch (error) {
      toast.error(aiErrorMessage(error));
    }
  };

  const confirmBatch = async (previewId: string) => {
    setPreviewBusy(true);
    try {
      await enqueueAiAnalysis({ preview_id: previewId });
      setPreview(null);
      toast.success(t("ai.manager.batchEnqueued"));
      refresh();
    } catch (error) {
      toast.error(aiErrorMessage(error));
    } finally {
      setPreviewBusy(false);
    }
  };

  const runAction = async (action: () => Promise<unknown>, successKey: string) => {
    try {
      await action();
      toast.success(t(successKey));
      refresh();
    } catch (error) {
      toast.error(aiErrorMessage(error));
    }
  };

  const startRetry = async (job: AiJobDto) => {
    try {
      const next = await previewAiAnalysis({ targets: [job.target], mode: "force" });
      setRetryJob(job);
      setPreview(next);
    } catch (error) {
      toast.error(aiErrorMessage(error));
    }
  };

  const confirmRetry = async (previewId: string) => {
    if (!retryJob) return;
    setPreviewBusy(true);
    try {
      await retryAiAnalysisJob({ job_id: retryJob.id, preview_id: previewId });
      setPreview(null);
      setRetryJob(null);
      toast.success(t("ai.manager.retryEnqueued"));
      refresh();
    } catch (error) {
      toast.error(aiErrorMessage(error));
    } finally {
      setPreviewBusy(false);
    }
  };

  if (loading && !stats) {
    return <div className="mt-20 text-center text-[13px] text-muted">{t("common.loading")}</div>;
  }

  return (
    <div className="space-y-6 p-6">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div>
          <h1 className="text-[18px] font-semibold text-primary">{t("ai.manager.title")}</h1>
          <p className="mt-0.5 text-[12.5px] text-muted">{t("ai.manager.subtitle")}</p>
        </div>
        <button
          type="button"
          onClick={startBatchPreview}
          className="inline-flex items-center gap-1.5 rounded-lg bg-accent px-3 py-1.5 text-[13px] font-medium text-white transition-colors hover:bg-accent-dark"
        >
          <Plus className="h-3.5 w-3.5" />
          {t("ai.manager.newBatch")}
        </button>
      </div>

      {stats && (
        <div className="grid grid-cols-2 gap-2 md:grid-cols-4 lg:grid-cols-7">
          {[
            { label: t("ai.manager.statTotal"), value: stats.targets_total },
            { label: t("ai.manager.statUnparsed"), value: stats.targets_unparsed },
            { label: t("ai.manager.statSucceeded"), value: stats.targets_succeeded },
            { label: t("ai.manager.statStale"), value: stats.targets_stale },
            { label: t("ai.manager.statFailed"), value: stats.targets_failed },
            { label: t("ai.manager.statNoDocument"), value: stats.targets_no_document },
            { label: t("ai.manager.statUnreadable"), value: stats.targets_unreadable },
          ].map((item) => (
            <div key={item.label} className="rounded-xl border border-border-subtle bg-surface/70 p-3">
              <div className="text-[22px] font-semibold text-primary">{item.value}</div>
              <div className="mt-0.5 text-[11.5px] text-muted">{item.label}</div>
            </div>
          ))}
        </div>
      )}

      <section className="rounded-xl border border-border-subtle bg-surface/70">
        <div className="border-b border-border-subtle px-4 py-3 text-[13px] font-semibold text-secondary">
          {t("ai.manager.batches")}
        </div>
        <div className="max-h-72 overflow-y-auto">
          {batches.length === 0 ? (
            <div className="px-4 py-8 text-center text-[12.5px] text-muted">{t("ai.manager.noBatches")}</div>
          ) : (
            batches.map((batch) => {
              const active = batch.id === selectedBatchId;
              const progress =
                batch.progress_total > 0
                  ? Math.round((batch.progress_completed / batch.progress_total) * 100)
                  : 0;
              return (
                <button
                  key={batch.id}
                  type="button"
                  onClick={() => setSelectedBatchId(batch.id)}
                  className={`flex w-full items-center gap-3 border-b border-border-subtle px-4 py-2.5 text-left last:border-b-0 ${
                    active ? "bg-surface-hover" : "hover:bg-surface-hover/50"
                  }`}
                >
                  <div className="min-w-0 flex-1">
                    <div className="flex items-center gap-2">
                      <span className="text-[13px] font-medium text-secondary">{batch.id.slice(0, 8)}</span>
                      <span className="rounded-full bg-surface-hover px-2 py-0.5 text-[11px] text-muted">
                        {batchStatusLabel(t, batch.status)}
                      </span>
                    </div>
                    <div className="mt-1 h-1.5 overflow-hidden rounded-full bg-bg-secondary">
                      <div className="h-full bg-accent" style={{ width: `${progress}%` }} />
                    </div>
                    <div className="mt-0.5 text-[11px] text-faint">
                      {batch.progress_completed}/{batch.progress_total} · {t("ai.manager.attempts")}:{" "}
                      {batch.jobs_succeeded + batch.jobs_failed + batch.jobs_cancelled}
                    </div>
                  </div>
                  <div className="flex shrink-0 items-center gap-1" onClick={(event) => event.stopPropagation()}>
                    {batch.status === "queued" || batch.status === "running" ? (
                      <button
                        type="button"
                        onClick={() => runAction(() => pauseAiAnalysisBatch({ batch_id: batch.id }), "ai.manager.paused")}
                        className="rounded p-1 text-muted transition-colors hover:text-secondary"
                        title={t("ai.manager.pause")}
                      >
                        <Pause className="h-3.5 w-3.5" />
                      </button>
                    ) : null}
                    {batch.status === "paused" ? (
                      <button
                        type="button"
                        onClick={() => runAction(() => resumeAiAnalysisBatch({ batch_id: batch.id }), "ai.manager.resumed")}
                        className="rounded p-1 text-muted transition-colors hover:text-secondary"
                        title={t("ai.manager.resume")}
                      >
                        <Play className="h-3.5 w-3.5" />
                      </button>
                    ) : null}
                    {batch.status === "queued" || batch.status === "running" || batch.status === "paused" ? (
                      <button
                        type="button"
                        onClick={() => runAction(() => cancelAiAnalysisBatch({ batch_id: batch.id }), "ai.manager.cancelled")}
                        className="rounded p-1 text-muted transition-colors hover:text-red-500"
                        title={t("ai.manager.cancel")}
                      >
                        <XCircle className="h-3.5 w-3.5" />
                      </button>
                    ) : null}
                  </div>
                </button>
              );
            })
          )}
        </div>
      </section>

      <section className="rounded-xl border border-border-subtle bg-surface/70">
        <div className="border-b border-border-subtle px-4 py-3 text-[13px] font-semibold text-secondary">
          {t("ai.manager.jobs")}
        </div>
        {jobs.length === 0 ? (
          <div className="px-4 py-8 text-center text-[12.5px] text-muted">{t("ai.manager.noJobs")}</div>
        ) : (
          <div className="max-h-96 overflow-y-auto">
            {jobs.map((job) => (
              <div
                key={job.id}
                className="flex items-center gap-3 border-b border-border-subtle px-4 py-2.5 text-[12.5px] last:border-b-0"
              >
                <span className="min-w-0 flex-1 truncate text-secondary">{job.skill_name}</span>
                <span className="shrink-0 rounded-full bg-surface-hover px-2 py-0.5 text-[11px] text-muted">
                  {jobStatusLabel(t, job.status)}
                </span>
                <span className="shrink-0 text-[11px] text-faint">
                  {job.attempt_count}/3
                </span>
                {job.error_message && (
                  <span className="hidden shrink-0 max-w-[220px] truncate text-[11px] text-red-500 lg:inline">
                    {job.error_message}
                  </span>
                )}
                <div className="flex shrink-0 items-center gap-1">
                  {job.status === "queued" || job.status === "retry_wait" || job.status === "interrupted" ? (
                    <button
                      type="button"
                      onClick={() => runAction(() => cancelAiAnalysisJob({ job_id: job.id }), "ai.manager.jobCancelled")}
                      className="rounded p-1 text-muted transition-colors hover:text-red-500"
                      title={t("ai.manager.cancelJob")}
                    >
                      <XCircle className="h-3.5 w-3.5" />
                    </button>
                  ) : null}
                  {job.status === "failed" ? (
                    <button
                      type="button"
                      onClick={() => startRetry(job)}
                      className="rounded p-1 text-muted transition-colors hover:text-secondary"
                      title={t("ai.manager.retry")}
                    >
                      <RotateCcw className="h-3.5 w-3.5" />
                    </button>
                  ) : null}
                </div>
              </div>
            ))}
          </div>
        )}
      </section>

      <AiAnalysisPreviewDialog
        preview={preview}
        busy={previewBusy}
        onClose={() => {
          setPreview(null);
          setRetryJob(null);
        }}
        onConfirm={retryJob ? confirmRetry : confirmBatch}
      />
    </div>
  );
}

function batchStatusLabel(t: (key: string) => string, status: AiBatchDto["status"]) {
  const key = `ai.batchStatus.${status}`;
  return t(key);
}

function jobStatusLabel(t: (key: string) => string, status: AiJobDto["status"]) {
  const key = `ai.jobStatus.${status}`;
  return t(key);
}

function aiErrorMessage(error: unknown) {
  if (isAiCommandError(error)) return error.message;
  return getErrorMessage(error, "");
}
