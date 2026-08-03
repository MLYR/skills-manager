import { useCallback, useEffect, useRef, useState } from "react";
import { confirm as dialogConfirm } from "@tauri-apps/plugin-dialog";
import {
  AlertCircle,
  CheckCircle2,
  KeyRound,
  Loader2,
  Save,
  ShieldCheck,
  Trash2,
  Wifi,
} from "lucide-react";
import { useTranslation } from "react-i18next";
import { toast } from "sonner";
import { isAiCommandError } from "../../lib/error";
import * as api from "../../lib/tauri";
import type {
  AiConfigDto,
  AiConfigInput,
  AiConnectionTestDto,
  AiOutputLanguage,
  AiProvider,
  AiProviderPresetDto,
} from "../../lib/tauri";

const AI_PROVIDERS: readonly AiProvider[] = [
  "openai",
  "deepseek",
  "openrouter",
  "ollama",
  "custom",
];
const AI_OUTPUT_LANGUAGES: readonly AiOutputLanguage[] = ["auto", "zh", "zh-TW", "en"];
const MAX_PRICE_MICROS_PER_MILLION = 1_000_000_000_000_000;

interface AiConfigForm {
  provider: AiProvider;
  baseUrl: string;
  model: string;
  outputLanguage: AiOutputLanguage;
  timeoutSeconds: string;
  concurrency: string;
  logRetentionDays: string;
  inputPrice: string;
  outputPrice: string;
}

interface Feedback {
  kind: "success" | "error";
  message: string;
}

type ConfigBuildResult =
  | { config: AiConfigInput; errorKey: null }
  | { config: null; errorKey: string };

const fieldClass =
  "h-8 w-full rounded-[4px] border border-border-subtle bg-background px-2.5 text-[13px] text-secondary outline-none transition-colors focus:border-border disabled:opacity-60";
const buttonClass =
  "inline-flex h-8 items-center justify-center gap-1.5 rounded-[4px] border px-2.5 text-[13px] font-medium transition-colors outline-none disabled:cursor-not-allowed disabled:opacity-60";

function isAiProvider(value: string): value is AiProvider {
  return AI_PROVIDERS.some((provider) => provider === value);
}

function isAiOutputLanguage(value: string): value is AiOutputLanguage {
  return AI_OUTPUT_LANGUAGES.some((language) => language === value);
}

function configToForm(config: AiConfigDto): AiConfigForm {
  return {
    provider: config.provider,
    baseUrl: config.base_url,
    model: config.model,
    outputLanguage: config.output_language,
    timeoutSeconds: String(config.timeout_seconds),
    concurrency: String(config.concurrency),
    logRetentionDays: String(config.log_retention_days),
    inputPrice: config.input_price_micros_per_million?.toString() ?? "",
    outputPrice: config.output_price_micros_per_million?.toString() ?? "",
  };
}

function parseRequiredInteger(value: string, min: number, max: number): number | null {
  const parsed = Number(value.trim());
  return Number.isSafeInteger(parsed) && parsed >= min && parsed <= max ? parsed : null;
}

function parseOptionalPrice(value: string): { valid: boolean; value: number | null } {
  const trimmed = value.trim();
  if (!trimmed) return { valid: true, value: null };
  const parsed = Number(trimmed);
  return {
    valid:
      Number.isSafeInteger(parsed) &&
      parsed >= 0 &&
      parsed <= MAX_PRICE_MICROS_PER_MILLION,
    value: Number.isSafeInteger(parsed) ? parsed : null,
  };
}

// Validate and normalize every field before any command invocation; the Rust
// backend remains authoritative for URL policy and all security checks.
function buildConfig(form: AiConfigForm): ConfigBuildResult {
  if (!isAiProvider(form.provider)) {
    return { config: null, errorKey: "providerInvalid" };
  }
  if (!isAiOutputLanguage(form.outputLanguage)) {
    return { config: null, errorKey: "languageInvalid" };
  }
  const baseUrl = form.baseUrl.trim();
  if (!baseUrl) return { config: null, errorKey: "baseUrlRequired" };
  const model = form.model.trim();
  if (!model) return { config: null, errorKey: "modelRequired" };

  const timeoutSeconds = parseRequiredInteger(form.timeoutSeconds, 1, 300);
  if (timeoutSeconds === null) return { config: null, errorKey: "timeoutInvalid" };
  const concurrency = parseRequiredInteger(form.concurrency, 1, 5);
  if (concurrency === null) return { config: null, errorKey: "concurrencyInvalid" };
  const logRetentionDays = parseRequiredInteger(form.logRetentionDays, 1, 3650);
  if (logRetentionDays === null) return { config: null, errorKey: "retentionInvalid" };

  const inputPrice = parseOptionalPrice(form.inputPrice);
  if (!inputPrice.valid) return { config: null, errorKey: "inputPriceInvalid" };
  const outputPrice = parseOptionalPrice(form.outputPrice);
  if (!outputPrice.valid) return { config: null, errorKey: "outputPriceInvalid" };

  return {
    config: {
      provider: form.provider,
      base_url: baseUrl,
      model,
      output_language: form.outputLanguage,
      timeout_seconds: timeoutSeconds,
      concurrency,
      log_retention_days: logRetentionDays,
      input_price_micros_per_million: inputPrice.value,
      output_price_micros_per_million: outputPrice.value,
    },
    errorKey: null,
  };
}

export function AiSettingsSection() {
  const { t } = useTranslation();
  const apiKeyInputRef = useRef<HTMLInputElement>(null);
  const [presets, setPresets] = useState<AiProviderPresetDto[]>([]);
  const [form, setForm] = useState<AiConfigForm | null>(null);
  const [hasApiKey, setHasApiKey] = useState(false);
  const [loading, setLoading] = useState(true);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [savingConfig, setSavingConfig] = useState(false);
  const [savingApiKey, setSavingApiKey] = useState(false);
  const [deletingApiKey, setDeletingApiKey] = useState(false);
  const [testing, setTesting] = useState<"local" | "paid" | null>(null);
  const [feedback, setFeedback] = useState<Feedback | null>(null);
  const [testResult, setTestResult] = useState<AiConnectionTestDto | null>(null);

  const errorMessage = useCallback(
    (error: unknown) => {
      if (isAiCommandError(error)) {
        return t(`settings.ai.errors.${error.code}`);
      }
      return t("settings.ai.errors.unknown");
    },
    [t],
  );

  const loadSettings = useCallback(async () => {
    setLoading(true);
    setLoadError(null);
    try {
      const [providerPresets, config, keyStatus] = await Promise.all([
        api.getAiProviderPresets(),
        api.getAiConfig(),
        api.getAiApiKeyStatus(),
      ]);
      // Reject malformed serialized enums rather than silently replacing a
      // potentially cost-sensitive provider configuration.
      if (
        !isAiProvider(config.provider) ||
        !isAiOutputLanguage(config.output_language) ||
        providerPresets.some((preset) => !isAiProvider(preset.id))
      ) {
        setLoadError(t("settings.ai.errors.invalid_config"));
        return;
      }
      setPresets(providerPresets);
      setForm(configToForm(config));
      setHasApiKey(keyStatus.has_api_key);
    } catch (error) {
      setLoadError(errorMessage(error));
    } finally {
      setLoading(false);
    }
  }, [errorMessage, t]);

  useEffect(() => {
    void loadSettings();
  }, [loadSettings]);

  const showValidationError = (errorKey: string) => {
    const message = t(`settings.ai.validation.${errorKey}`);
    setFeedback({ kind: "error", message });
    toast.error(message);
  };

  const handleProviderChange = (provider: AiProvider) => {
    const preset = presets.find((item) => item.id === provider);
    // Provider changes intentionally apply the frozen preset URL and model so
    // an old vendor endpoint is never submitted under a newly selected vendor.
    setForm((current) =>
      current
        ? {
            ...current,
            provider,
            baseUrl: preset?.base_url ?? "",
            model: preset?.default_model ?? "",
          }
        : current,
    );
    setFeedback(null);
    setTestResult(null);
  };

  const handleSaveConfig = async () => {
    if (!form) return;
    const result = buildConfig(form);
    if (!result.config) {
      showValidationError(result.errorKey);
      return;
    }

    setSavingConfig(true);
    setFeedback(null);
    try {
      const saved = await api.saveAiConfig(result.config);
      setForm(configToForm(saved));
      setHasApiKey(saved.has_api_key);
      setTestResult(null);
      const message = t("settings.ai.configSaved");
      setFeedback({ kind: "success", message });
      toast.success(message);
    } catch (error) {
      const message = errorMessage(error);
      setFeedback({ kind: "error", message });
      toast.error(message);
    } finally {
      setSavingConfig(false);
    }
  };

  const handleSaveApiKey = async () => {
    let apiKey: string | null = null;
    let input: { api_key: string } | null = null;
    setSavingApiKey(true);
    setFeedback(null);
    try {
      // The credential enters memory only at invocation time and is never
      // copied into React state, storage, logs, feedback, or a controlled prop.
      apiKey = apiKeyInputRef.current?.value ?? "";
      // Whitespace normalization is only a presence check: the exact original
      // credential must be written to Keyring without altering valid bytes.
      if (!apiKey.trim()) {
        showValidationError("apiKeyRequired");
        return;
      }
      input = { api_key: apiKey };
      const status = await api.setAiApiKey(input);
      setHasApiKey(status.has_api_key);
      const message = t("settings.ai.apiKeySaved");
      setFeedback({ kind: "success", message });
      toast.success(message);
    } catch (error) {
      const message = errorMessage(error);
      setFeedback({ kind: "error", message });
      toast.error(message);
    } finally {
      if (apiKeyInputRef.current) apiKeyInputRef.current.value = "";
      input = null;
      apiKey = null;
      setSavingApiKey(false);
    }
  };

  const handleDeleteApiKey = async () => {
    const confirmed = await dialogConfirm(t("settings.ai.apiKeyDeleteConfirm"), {
      title: t("settings.ai.apiKeyDeleteTitle"),
      kind: "warning",
    });
    if (!confirmed) return;

    setDeletingApiKey(true);
    setFeedback(null);
    try {
      const status = await api.deleteAiApiKey();
      setHasApiKey(status.has_api_key);
      const message = t("settings.ai.apiKeyDeleted");
      setFeedback({ kind: "success", message });
      toast.success(message);
    } catch (error) {
      const message = errorMessage(error);
      setFeedback({ kind: "error", message });
      toast.error(message);
    } finally {
      if (apiKeyInputRef.current) apiKeyInputRef.current.value = "";
      setDeletingApiKey(false);
    }
  };

  const runConnectionTest = async (confirmBillableRequest: boolean) => {
    if (!form) return;
    const built = buildConfig(form);
    if (!built.config) {
      showValidationError(built.errorKey);
      return;
    }

    setTesting(confirmBillableRequest ? "paid" : "local");
    setFeedback(null);
    setTestResult(null);
    try {
      const result = await api.testAiConnection({
        config: built.config,
        confirm_billable_request: confirmBillableRequest,
      });
      setTestResult(result);
      let message: string;
      if (result.success) {
        message = result.billable_request_sent
          ? t("settings.ai.connectionSuccess")
          : t("settings.ai.localValidationSuccess");
      } else if (!result.billable_request_sent) {
        // Billing state is authoritative: a null HTTP status alone cannot tell
        // whether transmission failed before or after the request was sent.
        message = t("settings.ai.connectionRequestNotSent");
      } else if (result.http_status === null) {
        message = t("settings.ai.connectionSendAttemptedNoResponse");
      } else {
        message = t("settings.ai.connectionFailureStatus", { status: result.http_status });
      }
      const kind = result.success ? "success" : "error";
      setFeedback({ kind, message });
      if (result.success) toast.success(message);
      else toast.error(message);
    } catch (error) {
      const message = errorMessage(error);
      setFeedback({ kind: "error", message });
      toast.error(message);
    } finally {
      setTesting(null);
    }
  };

  const handlePaidConnectionTest = async () => {
    const confirmed = await dialogConfirm(t("settings.ai.paidTestConfirm"), {
      title: t("settings.ai.paidTestConfirmTitle"),
      kind: "warning",
    });
    if (confirmed) await runConnectionTest(true);
  };

  const selectedPreset = form
    ? presets.find((preset) => preset.id === form.provider)
    : undefined;
  const requiresApiKey = selectedPreset?.api_key_required ?? form?.provider !== "ollama";
  const isBusy = savingConfig || savingApiKey || deletingApiKey || testing !== null;

  return (
    <section>
      <h2 className="app-section-title mb-3">{t("settings.ai.title")}</h2>
      <div className="app-panel overflow-hidden">
        {loading ? (
          <div className="flex items-center gap-2 px-4 py-5 text-[13px] text-muted" role="status">
            <Loader2 className="h-4 w-4 animate-spin" />
            {t("settings.ai.loading")}
          </div>
        ) : loadError || !form ? (
          <div className="flex flex-wrap items-center justify-between gap-3 px-4 py-4" role="alert">
            <div className="flex min-w-0 items-start gap-2 text-[13px] text-red-500">
              <AlertCircle className="mt-0.5 h-4 w-4 shrink-0" />
              <span>{loadError ?? t("settings.ai.errors.unknown")}</span>
            </div>
            <button
              type="button"
              onClick={() => void loadSettings()}
              className={`${buttonClass} border-border-subtle text-tertiary hover:bg-surface-hover`}
            >
              {t("settings.ai.retryLoad")}
            </button>
          </div>
        ) : (
          <div className="divide-y divide-border-subtle">
            <div className="space-y-3 px-4 py-3">
              <div>
                <h3 className="text-[13px] font-medium text-secondary">{t("settings.ai.configTitle")}</h3>
                <p className="mt-0.5 text-[12px] text-muted">{t("settings.ai.configDesc")}</p>
              </div>

              <div className="grid grid-cols-1 gap-3 md:grid-cols-2">
                <label className="block text-[12px] text-muted">
                  <span className="mb-1 block">{t("settings.ai.provider")}</span>
                  <select
                    value={form.provider}
                    onChange={(event) => {
                      if (isAiProvider(event.target.value)) handleProviderChange(event.target.value);
                    }}
                    disabled={isBusy}
                    className={`${fieldClass} appearance-none`}
                  >
                    {presets.map((preset) => (
                      <option key={preset.id} value={preset.id}>
                        {preset.display_name}
                      </option>
                    ))}
                  </select>
                </label>

                <label className="block text-[12px] text-muted">
                  <span className="mb-1 block">{t("settings.ai.model")}</span>
                  <input
                    type="text"
                    value={form.model}
                    onChange={(event) => setForm({ ...form, model: event.target.value })}
                    disabled={isBusy}
                    placeholder={t("settings.ai.modelPlaceholder")}
                    className={fieldClass}
                    spellCheck={false}
                  />
                </label>

                <label className="block text-[12px] text-muted md:col-span-2">
                  <span className="mb-1 block">{t("settings.ai.baseUrl")}</span>
                  <input
                    type="text"
                    inputMode="url"
                    value={form.baseUrl}
                    onChange={(event) => setForm({ ...form, baseUrl: event.target.value })}
                    disabled={isBusy}
                    placeholder={t("settings.ai.baseUrlPlaceholder")}
                    className={`${fieldClass} font-mono`}
                    autoCapitalize="none"
                    spellCheck={false}
                  />
                </label>

                <label className="block text-[12px] text-muted">
                  <span className="mb-1 block">{t("settings.ai.outputLanguage")}</span>
                  <select
                    value={form.outputLanguage}
                    onChange={(event) => {
                      if (isAiOutputLanguage(event.target.value)) {
                        setForm({ ...form, outputLanguage: event.target.value });
                      }
                    }}
                    disabled={isBusy}
                    className={`${fieldClass} appearance-none`}
                  >
                    <option value="auto">{t("settings.ai.languages.auto")}</option>
                    <option value="zh">{t("settings.ai.languages.zh")}</option>
                    <option value="zh-TW">{t("settings.ai.languages.zh-TW")}</option>
                    <option value="en">{t("settings.ai.languages.en")}</option>
                  </select>
                </label>

                <label className="block text-[12px] text-muted">
                  <span className="mb-1 block">{t("settings.ai.timeoutSeconds")}</span>
                  <input
                    type="number"
                    min={1}
                    max={300}
                    step={1}
                    value={form.timeoutSeconds}
                    onChange={(event) => setForm({ ...form, timeoutSeconds: event.target.value })}
                    disabled={isBusy}
                    className={fieldClass}
                  />
                </label>

                <label className="block text-[12px] text-muted">
                  <span className="mb-1 block">{t("settings.ai.concurrency")}</span>
                  <input
                    type="number"
                    min={1}
                    max={5}
                    step={1}
                    value={form.concurrency}
                    onChange={(event) => setForm({ ...form, concurrency: event.target.value })}
                    disabled={isBusy}
                    className={fieldClass}
                  />
                </label>

                <label className="block text-[12px] text-muted">
                  <span className="mb-1 block">{t("settings.ai.logRetentionDays")}</span>
                  <input
                    type="number"
                    min={1}
                    max={3650}
                    step={1}
                    value={form.logRetentionDays}
                    onChange={(event) => setForm({ ...form, logRetentionDays: event.target.value })}
                    disabled={isBusy}
                    className={fieldClass}
                  />
                </label>

                <label className="block text-[12px] text-muted">
                  <span className="mb-1 block">{t("settings.ai.inputPrice")}</span>
                  <input
                    type="number"
                    min={0}
                    max={MAX_PRICE_MICROS_PER_MILLION}
                    step={1}
                    value={form.inputPrice}
                    onChange={(event) => setForm({ ...form, inputPrice: event.target.value })}
                    disabled={isBusy}
                    placeholder={t("settings.ai.pricePlaceholder")}
                    className={fieldClass}
                  />
                </label>

                <label className="block text-[12px] text-muted">
                  <span className="mb-1 block">{t("settings.ai.outputPrice")}</span>
                  <input
                    type="number"
                    min={0}
                    max={MAX_PRICE_MICROS_PER_MILLION}
                    step={1}
                    value={form.outputPrice}
                    onChange={(event) => setForm({ ...form, outputPrice: event.target.value })}
                    disabled={isBusy}
                    placeholder={t("settings.ai.pricePlaceholder")}
                    className={fieldClass}
                  />
                </label>
              </div>

              <p className="text-[12px] text-muted">{t("settings.ai.priceDesc")}</p>
              <div className="flex justify-end">
                <button
                  type="button"
                  onClick={() => void handleSaveConfig()}
                  disabled={isBusy}
                  className={`${buttonClass} border-accent bg-accent text-white hover:opacity-90`}
                >
                  {savingConfig ? <Loader2 className="h-3.5 w-3.5 animate-spin" /> : <Save className="h-3.5 w-3.5" />}
                  {savingConfig ? t("settings.ai.savingConfig") : t("settings.ai.saveConfig")}
                </button>
              </div>
            </div>

            <div className="space-y-3 px-4 py-3">
              <div className="flex flex-wrap items-start justify-between gap-2">
                <div>
                  <h3 className="flex items-center gap-1.5 text-[13px] font-medium text-secondary">
                    <KeyRound className="h-3.5 w-3.5" />
                    {t("settings.ai.apiKeyTitle")}
                  </h3>
                  <p className="mt-0.5 text-[12px] text-muted">
                    {requiresApiKey ? t("settings.ai.apiKeyDesc") : t("settings.ai.apiKeyOptionalDesc")}
                  </p>
                </div>
                <span className={`rounded-full px-2 py-0.5 text-[11px] font-medium ${hasApiKey ? "bg-emerald-500/10 text-emerald-600" : "bg-surface-hover text-muted"}`}>
                  {hasApiKey ? t("settings.ai.apiKeyStored") : t("settings.ai.apiKeyMissing")}
                </span>
              </div>

              <div className="flex flex-wrap items-center gap-2">
                <div className="relative min-w-[220px] flex-1">
                  <input
                    ref={apiKeyInputRef}
                    type="password"
                    autoComplete="new-password"
                    spellCheck={false}
                    disabled={isBusy}
                    aria-label={t("settings.ai.apiKeyInputLabel")}
                    placeholder={hasApiKey ? t("settings.ai.apiKeyReplacePlaceholder") : t("settings.ai.apiKeyPlaceholder")}
                    className={`${fieldClass} peer pr-32 font-mono`}
                  />
                  {hasApiKey && (
                    // Show stored status inside the otherwise blank password field,
                    // but hide it as soon as a replacement key is being entered.
                    <span
                      aria-hidden="true"
                      className="pointer-events-none absolute inset-y-0 right-2 hidden items-center gap-1 text-[11px] font-medium text-emerald-600 peer-placeholder-shown:flex dark:text-emerald-400"
                    >
                      <ShieldCheck className="h-3.5 w-3.5" />
                      {t("settings.ai.apiKeyInputStoredHint")}
                    </span>
                  )}
                </div>
                <button
                  type="button"
                  onClick={() => void handleSaveApiKey()}
                  disabled={isBusy}
                  className={`${buttonClass} border-border-subtle text-tertiary hover:bg-surface-hover`}
                >
                  {savingApiKey ? <Loader2 className="h-3.5 w-3.5 animate-spin" /> : <ShieldCheck className="h-3.5 w-3.5" />}
                  {savingApiKey ? t("settings.ai.savingApiKey") : t("settings.ai.saveApiKey")}
                </button>
                <button
                  type="button"
                  onClick={() => void handleDeleteApiKey()}
                  disabled={isBusy || !hasApiKey}
                  className={`${buttonClass} border-red-500/25 text-red-500 hover:bg-red-500/5`}
                >
                  {deletingApiKey ? <Loader2 className="h-3.5 w-3.5 animate-spin" /> : <Trash2 className="h-3.5 w-3.5" />}
                  {deletingApiKey ? t("settings.ai.deletingApiKey") : t("settings.ai.deleteApiKey")}
                </button>
              </div>
              <p className="text-[12px] text-muted">{t("settings.ai.apiKeySecurityHint")}</p>
            </div>

            <div className="space-y-3 px-4 py-3">
              <div>
                <h3 className="text-[13px] font-medium text-secondary">{t("settings.ai.testTitle")}</h3>
                <p className="mt-0.5 text-[12px] text-muted">{t("settings.ai.testDesc")}</p>
              </div>
              <div className="flex flex-wrap items-center gap-2">
                <button
                  type="button"
                  onClick={() => void runConnectionTest(false)}
                  disabled={isBusy}
                  className={`${buttonClass} border-border-subtle text-tertiary hover:bg-surface-hover`}
                >
                  {testing === "local" ? <Loader2 className="h-3.5 w-3.5 animate-spin" /> : <ShieldCheck className="h-3.5 w-3.5" />}
                  {testing === "local" ? t("settings.ai.validatingLocally") : t("settings.ai.validateLocally")}
                </button>
                <button
                  type="button"
                  onClick={() => void handlePaidConnectionTest()}
                  disabled={isBusy || (requiresApiKey && !hasApiKey)}
                  className={`${buttonClass} border-accent-border bg-accent-bg text-accent hover:border-accent`}
                >
                  {testing === "paid" ? <Loader2 className="h-3.5 w-3.5 animate-spin" /> : <Wifi className="h-3.5 w-3.5" />}
                  {testing === "paid" ? t("settings.ai.testingConnection") : t("settings.ai.testConnection")}
                </button>
              </div>
              {requiresApiKey && !hasApiKey && (
                <p className="text-[12px] text-amber-600">{t("settings.ai.testNeedsApiKey")}</p>
              )}
              {testResult && (
                <p className="text-[12px] text-muted">
                  {t("settings.ai.testResultMeta", {
                    latency: testResult.latency_ms,
                    status:
                      testResult.http_status ??
                      (testResult.billable_request_sent
                        ? t("settings.ai.noHttpResponse")
                        : t("settings.ai.requestNotSent")),
                  })}
                </p>
              )}
            </div>

            {feedback && (
              <div
                className={`flex items-start gap-2 px-4 py-3 text-[13px] ${feedback.kind === "success" ? "text-emerald-600" : "text-red-500"}`}
                role={feedback.kind === "error" ? "alert" : "status"}
                aria-live="polite"
              >
                {feedback.kind === "success" ? (
                  <CheckCircle2 className="mt-0.5 h-4 w-4 shrink-0" />
                ) : (
                  <AlertCircle className="mt-0.5 h-4 w-4 shrink-0" />
                )}
                <span>{feedback.message}</span>
              </div>
            )}
          </div>
        )}
      </div>
    </section>
  );
}
