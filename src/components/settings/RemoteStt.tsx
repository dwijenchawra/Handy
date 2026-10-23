import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { commands } from "@/bindings";
import { useTranslation } from "react-i18next";
import { useSettingsStore } from "@/stores/settingsStore";

interface BenchmarkRow {
  recording: string;
  backend: string;
  model: string;
  queue_ms: number;
  processing_ms: number;
  e2e_ms: number;
  audio_seconds: number;
  cold_model: boolean;
  streaming?: boolean;
  success: boolean;
  error: string | null;
}

export function RemoteStt() {
  const { t } = useTranslation();
  const settings = useSettingsStore((state) => state.settings);
  const refresh = useSettingsStore((state) => state.refreshSettings);
  const [url, setUrl] = useState<string | null>(null);
  const [error, setError] = useState("");
  const [saving, setSaving] = useState(false);
  const endpoint = url ?? settings?.remote_stt_url ?? "";
  const [rows, setRows] = useState<BenchmarkRow[]>([]);
  async function refreshResults() {
    try {
      setRows(await invoke<BenchmarkRow[]>("get_stt_benchmarks"));
    } catch (cause) {
      setError(String(cause));
    }
  }
  useEffect(() => {
    void refreshResults();
  }, []);
  async function save(enabled: boolean, compare = false) {
    setSaving(true);
    setError("");
    try {
      const result = await commands.setRemoteStt(enabled, compare, endpoint);
      if (result.status === "error") throw new Error(result.error);
      await refresh();
    } catch (cause) {
      setError(String(cause));
    } finally {
      setSaving(false);
    }
  }
  return (
    <div className="p-4 space-y-2">
      <label className="block" htmlFor="remote-stt-url">
        {t("settings.remoteStt.label")}
      </label>
      <p className="text-sm text-text/60">
        {t("settings.remoteStt.description")}
      </p>
      <input
        id="remote-stt-url"
        type="url"
        className="w-full rounded border p-2 bg-transparent"
        value={endpoint}
        onChange={(event) => setUrl(event.target.value)}
        placeholder="wss://stt.dwijen.dev/stream"
        disabled={saving}
      />
      <div className="flex gap-4">
        <button
          disabled={saving || !endpoint.trim()}
          onClick={() => save(true)}
        >
          {t("settings.remoteStt.enable")}
        </button>
        <button
          disabled={saving || !endpoint.trim()}
          onClick={() => save(false, true)}
        >
          {t("settings.remoteStt.compare")}
        </button>
        <button disabled={saving} onClick={() => save(false)}>
          {t("settings.remoteStt.disable")}
        </button>
      </div>
      <p className="text-sm">
        {t(
          settings?.remote_stt_compare
            ? "settings.remoteStt.compareActive"
            : settings?.remote_stt_enabled
              ? "settings.remoteStt.remoteActive"
              : "settings.remoteStt.localActive",
        )}
      </p>
      <p className="text-sm text-text/60">
        {t("settings.remoteStt.timingDescription")}
      </p>
      <button onClick={refreshResults}>
        {t("settings.remoteStt.refreshResults")}
      </button>
      <div className="overflow-x-auto">
        <table className="w-full text-sm text-left">
          <thead>
            <tr>
              {[
                "recording",
                "backend",
                "e2e",
                "queue",
                "processing",
                "status",
              ].map((key) => (
                <th className="p-2" key={key}>
                  {t(`settings.remoteStt.columns.${key}`)}
                </th>
              ))}
            </tr>
          </thead>
          <tbody>
            {rows
              .slice(-20)
              .reverse()
              .map((row, index) => (
                <tr key={`${row.recording}-${row.backend}-${index}`}>
                  <td className="p-2" title={row.recording}>
                    {row.recording.replace("handy-", "").replace(".wav", "")}
                  </td>
                  <td className="p-2" title={row.model}>
                    {t(`settings.remoteStt.${row.backend}`)}{" "}
                    {row.streaming && t("settings.remoteStt.streaming")}
                  </td>
                  <td className="p-2">{(row.e2e_ms / 1000).toFixed(2)}</td>
                  <td className="p-2">{(row.queue_ms / 1000).toFixed(2)}</td>
                  <td className="p-2">
                    {(row.processing_ms / 1000).toFixed(2)}
                  </td>
                  <td className="p-2" title={row.error ?? ""}>
                    {t(
                      row.success
                        ? row.cold_model
                          ? "settings.remoteStt.cold"
                          : "settings.remoteStt.success"
                        : "settings.remoteStt.failed",
                    )}
                  </td>
                </tr>
              ))}
          </tbody>
        </table>
      </div>
      {error && <p role="alert">{error}</p>}
    </div>
  );
}
