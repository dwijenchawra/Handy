import { useState } from "react";
import { commands } from "@/bindings";
import { useTranslation } from "react-i18next";
import { useSettingsStore } from "@/stores/settingsStore";

export function RemoteStt() {
  const { t } = useTranslation();
  const settings = useSettingsStore((state) => state.settings);
  const refresh = useSettingsStore((state) => state.refreshSettings);
  const [url, setUrl] = useState<string | null>(null);
  const [error, setError] = useState("");
  const [saving, setSaving] = useState(false);
  const endpoint = url ?? settings?.remote_stt_url ?? "";
  async function save(enabled: boolean) {
    setSaving(true);
    setError("");
    try {
      const result = await commands.setRemoteStt(enabled, endpoint);
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
        <button disabled={saving} onClick={() => save(false)}>
          {t("settings.remoteStt.disable")}
        </button>
      </div>
      <p className="text-sm">
        {t(
          settings?.remote_stt_enabled
            ? "settings.remoteStt.remoteActive"
            : "settings.remoteStt.localActive",
        )}
      </p>
      {error && <p role="alert">{error}</p>}
    </div>
  );
}
