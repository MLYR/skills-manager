import { useCallback, useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { toast } from "sonner";
import { Pause, Play, XCircle, RotateCcw, Plus } from "lucide-react";
import { useApp } from "../context/AppContext";
import {
  cancelAiAnalysisBatch,
  cancelAiAnalysisJob,
  clearAiAnalysisLogs,
  enqueueAiAnalysis,
  getAiAnalysisLog,
  getAiAnalysisQueueStats,
  listAiAnalysisBatches,
  listAiAnalysisJobs,
  listAiAnalysisLogs,
  listAiAnalysisSummaries,
  pauseAiAnalysisBatch,
  previewAiAnalysis,
  resumeAiAnalysisBatch,
  retryAiAnalysisJob,
  type AiAnalysisPreviewDto,
  type AiBatchDto,
  type AiJobDto,
  type AiLogDetailDto,
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
  const [selectedJobId, setSelectedJobId] = useState<string | null>(null);
  const [jobLogs, setJobLogs] = useState<AiLogDetailDto[]>([]);
  const [jobLogsLoading, setJobLogsLoading] = useState(false);
  const [unparsedLoading, setUnparsedLoading] = useState(false);
  const [analysisCounts, setAnalysisCounts] = useState<{ parsed: number; unparsed: number } | null>(null);
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

  const loadUnparsedTargets = useCallback(async (): Promise<{
    targets: AiTargetRef[];
    counts: { parsed: number; unparsed: number };
  } | null> => {
    setUnparsedLoading(true);
    const targets: AiTargetRef[] = managedSkills.map((skill) => ({
      kind: "managed",
      skill_id: skill.id,
    }));
    try {
      if (targets.length === 0) {
        const counts = { parsed: 0, unparsed: 0 };
        setAnalysisCounts(counts);
        return { targets: [], counts };
      }
      const summaries = await listAiAnalysisSummaries({ targets });
      // 一键解析补齐没有有效解读的目标：失败项需要能经预览确认后重试，
      // 但成功、过期和活动任务仍不能再次产生请求。
      const next = summaries
        .filter((summary) => summary.status === "unparsed" || summary.status === "failed")
        .map((summary) => summary.target);
      const counts = {
        // 过期结果仍是已有解读，不会被“一键解析”重复请求。
        parsed: summaries.filter((summary) => summary.status === "succeeded" || summary.status === "stale").length,
        unparsed: next.length,
      };
      setAnalysisCounts(counts);
      return { targets: next, counts };
    } catch (error) {
      setAnalysisCounts(null);
      toast.error(aiErrorMessage(error));
      return null;
    } finally {
      setUnparsedLoading(false);
    }
  }, [managedSkills]);

  const refresh = useCallback(() => {
    requestRef.current += 1;
    const requestId = requestRef.current;
    getAiAnalysisQueueStats()
      .then((nextStats) => {
        if (requestId !== requestRef.current) return;
        setStats(nextStats);
      })
      .catch((error) => {
        if (requestId === requestRef.current) {
          toast.error(aiErrorMessage(error));
        }
      })
      .finally(() => {
        if (requestId === requestRef.current) setLoading(false);
    });
    // Eligibility is loaded independently because the full cross-workspace
    // statistics scan can be slow and must not disable this action.
    void loadUnparsedTargets();
    loadBatches();
  }, [loadBatches, loadUnparsedTargets]);

  useEffect(() => {
    refresh();
    // Real backend state only: the fast interval re-reads batches/jobs, never
    // the full filesystem-backed stats scan, and never fabricates progress.
    const timer = window.setInterval(loadBatches, 2_000);
    return () => {
      window.clearInterval(timer);
    };
  }, [refresh, loadBatches]);

  useEffect(() => {
    if (!selectedBatchId) {
      setJobs([]);
      setSelectedJobId(null);
      return;
    }
    listAiAnalysisJobs({ batch_id: selectedBatchId, limit: 100 })
      .then((page) => {
        setJobs(page.items);
        setSelectedJobId((current) =>
          current && page.items.some((job) => job.id === current)
            ? current
            : page.items[0]?.id ?? null,
        );
      })
      .catch((error) => toast.error(aiErrorMessage(error)));
  }, [selectedBatchId, batches]);

  useEffect(() => {
    if (!selectedJobId) {
      setJobLogs([]);
      return;
    }
    let cancelled = false;
    setJobLogsLoading(true);
    listAiAnalysisLogs({ job_id: selectedJobId, limit: 100 })
      .then(async (page) => {
        return Promise.all(page.items.map((log) => getAiAnalysisLog({ log_id: log.id })));
      })
      .then((details) => {
        if (!cancelled) setJobLogs(details.sort((a, b) => b.created_at - a.created_at));
      })
      .catch((error) => {
        if (!cancelled) toast.error(aiErrorMessage(error));
      })
      .finally(() => {
        if (!cancelled) setJobLogsLoading(false);
      });
    return () => {
      cancelled = true;
    };
  }, [selectedJobId]);

  const startBatchPreview = async () => {
    if (unparsedLoading) return;
    const loaded = await loadUnparsedTargets();
    if (!loaded) return;
    if (loaded.targets.length === 0) {
      toast.success(t("ai.manager.noUnparsed", loaded.counts));
      return;
    }
    try {
      const next = await previewAiAnalysis({ targets: loaded.targets, mode: "missing_only" });
      if (next.valid_documents === 0) {
        return;
      }
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

  const handleClearLogs = async () => {
    try {
      const result = await clearAiAnalysisLogs();
      toast.success(t("ai.logs.cleared", { count: result.deleted_count }));
      setJobLogs([]);
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

  const selectedJob = jobs.find((job) => job.id === selectedJobId) ?? null;
  const requestLog =
    jobLogs.find((log) => log.event_kind === "request_started" || log.event_kind === "correction_requested") ??
    null;
  // JSON/Schema 失败会先记录真实响应、再记录终态错误；优先展示前者，
  // 否则按时间排序时通用错误会把实际服务商响应遮住。
  const responseLog =
    jobLogs.find((log) => log.event_kind === "response_received") ??
    jobLogs.find((log) => ["request_failed", "retry_scheduled", "cancelled"].includes(log.event_kind)) ??
    null;
  const responseError = jobLogs.find((log) => Boolean(log.error_message)) ?? null;

  return (
    <div className="space-y-6 p-6">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div>
          <h1 className="text-[18px] font-semibold text-primary">{t("ai.manager.title")}</h1>
          <p className="mt-0.5 text-[12.5px] text-muted">{t("ai.manager.subtitle")}</p>
        </div>
        <div className="flex flex-wrap items-center justify-end gap-2">
          <button
            type="button"
            onClick={startBatchPreview}
            aria-busy={unparsedLoading}
            className="inline-flex items-center gap-1.5 rounded-lg bg-accent px-3 py-1.5 text-[13px] font-medium text-white transition-colors hover:bg-accent-dark"
          >
            <Plus className="h-3.5 w-3.5" />
            {unparsedLoading ? t("ai.manager.loadingTargets") : t("ai.manager.newBatch")}
          </button>
        </div>
      </div>

      {stats ? (
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
      ) : (
        <div className="rounded-xl border border-border-subtle bg-surface/50 px-3 py-2 text-[12px] text-muted">
          {loading ? t("ai.manager.statsLoading") : t("ai.manager.statsUnavailable")}
          {analysisCounts ? (
            <span className="ml-2 text-secondary">
              {t("ai.preview.analysisCounts", analysisCounts)}
            </span>
          ) : null}
        </div>
      )}

      <div className="grid gap-4 lg:grid-cols-[minmax(260px,0.9fr)_minmax(0,1.5fr)]">
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
                <div
                  key={batch.id}
                  role="button"
                  tabIndex={0}
                  onClick={() => setSelectedBatchId(batch.id)}
                  onKeyDown={(event) => {
                    if (event.key === "Enter" || event.key === " ") setSelectedBatchId(batch.id);
                  }}
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
                    <div className="mt-1 flex flex-wrap gap-x-2 text-[11px] text-faint">
                      <span>{batch.progress_completed}/{batch.progress_total}</span>
                      <span className="text-green-600 dark:text-green-400">
                        {t("ai.manager.succeededCount", { count: batch.jobs_succeeded })}
                      </span>
                      <span className={batch.jobs_failed > 0 ? "text-red-500" : "text-faint"}>
                        {t("ai.manager.failedCount", { count: batch.jobs_failed })}
                      </span>
                      {batch.jobs_cancelled > 0 ? (
                        <span>{t("ai.manager.cancelledCount", { count: batch.jobs_cancelled })}</span>
                      ) : null}
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
                </div>
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
                role="button"
                tabIndex={0}
                onClick={() => setSelectedJobId(job.id)}
                onKeyDown={(event) => {
                  if (event.key === "Enter" || event.key === " ") setSelectedJobId(job.id);
                }}
                className={`flex items-center gap-3 border-b border-border-subtle px-4 py-2.5 text-[12.5px] last:border-b-0 ${
                  selectedJobId === job.id ? "bg-surface-hover" : "hover:bg-surface-hover/50"
                }`}
              >
                <span className="min-w-0 flex-1 truncate text-secondary">{job.skill_name}</span>
                <span className="shrink-0 rounded-full bg-surface-hover px-2 py-0.5 text-[11px] text-muted">
                  {jobStatusLabel(t, job.status)}
                </span>
                <span className="shrink-0 text-[11px] text-faint">
                  {job.attempt_count}/3
                </span>
                {job.error_message && (job.status === "failed" || job.status === "retry_wait") && (
                  <span className="hidden max-w-[220px] shrink-0 truncate text-[11px] text-red-500 lg:inline">
                    {job.error_message}
                  </span>
                )}
                <div className="flex shrink-0 items-center gap-1" onClick={(event) => event.stopPropagation()}>
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
      </div>

      <section className="rounded-xl border border-border-subtle bg-surface/70">
        <div className="flex items-center justify-between border-b border-border-subtle px-4 py-3">
          <span className="text-[13px] font-semibold text-secondary">{t("ai.logs.title")}</span>
          <button
            type="button"
            onClick={handleClearLogs}
            className="rounded-full bg-surface-hover px-2.5 py-1 text-[12px] font-medium text-muted transition-colors hover:text-red-500"
          >
            {t("ai.logs.clear")}
          </button>
        </div>
        {!selectedJobId ? (
          <div className="px-4 py-8 text-center text-[12.5px] text-muted">{t("ai.logs.selectTask")}</div>
        ) : jobLogsLoading ? (
          <div className="px-4 py-8 text-center text-[12.5px] text-muted">{t("ai.logs.loading")}</div>
        ) : jobLogs.length === 0 ? (
          <div className="px-4 py-8 text-center text-[12.5px] text-muted">{t("ai.logs.empty")}</div>
        ) : (
          <div className="space-y-3 px-4 py-3">
            <div className="flex items-center justify-between text-[12px] font-semibold text-secondary">
              <span>{selectedJob?.skill_name ?? selectedJobId.slice(0, 8)}</span>
              <span className="text-[11px] font-normal text-faint">{t("ai.logs.detail")}</span>
            </div>
            <div className="grid gap-3 md:grid-cols-2">
              <LogPane
                title={t("ai.logs.request")}
                content={requestLog ? [requestLog.request_system_prompt, requestLog.request_user_prompt].filter(Boolean).join("\n\n") : null}
              />
              <LogPane
                title={t("ai.logs.response")}
                content={
                  responseLog
                    ? [
                        responseLog.raw_response,
                        responseLog.error_message,
                        responseError?.id === responseLog.id ? null : responseError?.error_message,
                      ]
                        .filter(Boolean)
                        .join("\n\n")
                    : null
                }
              />
            </div>
            <div className="flex flex-wrap gap-2 text-[11px] text-faint">
              {jobLogs.map((log) => (
                <span key={log.id} className="rounded-full bg-surface-hover px-2 py-0.5">
                  {log.event_kind} · {new Date(log.created_at).toLocaleTimeString()}
                </span>
              ))}
            </div>
          </div>
        )}
      </section>

      <AiAnalysisPreviewDialog
        preview={preview}
        busy={previewBusy}
        analysisCounts={retryJob ? null : analysisCounts}
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

function LogPane({ title, content }: { title: string; content: string | null }) {
  return (
    <div className="min-w-0">
      <div className="mb-1 text-[11px] font-medium text-faint">{title}</div>
      <pre className="max-h-64 min-h-28 overflow-y-auto whitespace-pre-wrap rounded bg-bg-secondary p-2 text-[11.5px] leading-[16px] text-muted">
        {content || "—"}
      </pre>
    </div>
  );
}

function aiErrorMessage(error: unknown) {
  if (isAiCommandError(error)) return error.message;
  return getErrorMessage(error, "");
}
