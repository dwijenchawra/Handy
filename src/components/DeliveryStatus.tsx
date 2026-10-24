import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { useTranslation } from "react-i18next";

type Status = {
  current: { id: string; text: string } | null;
  count: number;
  pasting: boolean;
};
export default function DeliveryStatus() {
  const { t } = useTranslation();
  const [status, setStatus] = useState<Status | null>(null);
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);
  useEffect(() => {
    let disposed = false;
    let cleanup: (() => void) | undefined;
    void (async () => {
      const unlisten = await listen<Status>("delivery-status", (e) => {
        if (!disposed) setStatus(e.payload);
      });
      if (disposed) {
        unlisten();
        return;
      }
      cleanup = unlisten;
      const current = await invoke<Status>("get_delivery_status");
      if (!disposed) setStatus(current);
    })().catch((e) => {
      if (!disposed) setError(String(e));
    });
    return () => {
      disposed = true;
      cleanup?.();
    };
  }, []);
  const act = async (action: string) => {
    if (!status?.current) return;
    setBusy(true);
    setError("");
    try {
      await invoke("resolve_delivery", { id: status.current.id, action });
      setStatus(await invoke<Status>("get_delivery_status"));
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };
  if (!status?.current && !error) return null;
  return (
    <section
      className="w-full rounded-lg border border-mid-gray/30 p-4"
      aria-live="polite"
    >
      {status?.current && (
        <>
          <p className="font-semibold">
            {t(status.pasting ? "delivery.pasting" : "delivery.ready")}
          </p>
          <p className="text-sm">
            {t("delivery.instructions", { count: status.count })}
          </p>
          <p className="my-2 max-h-32 overflow-auto whitespace-pre-wrap text-sm">
            {status.current.text}
          </p>
          <div className="flex flex-wrap gap-3">
            <button
              className="rounded-md border border-mid-gray/30 px-3 py-1 disabled:opacity-50"
              disabled={busy || status.pasting}
              onClick={() => void act("copy")}
            >
              {t("delivery.copy")}
            </button>
            <button
              className="rounded-md border border-mid-gray/30 px-3 py-1 disabled:opacity-50"
              disabled={busy || status.pasting}
              onClick={() => void act("retry")}
            >
              {t("delivery.retry")}
            </button>
            <button
              className="rounded-md border border-mid-gray/30 px-3 py-1 disabled:opacity-50"
              disabled={busy || status.pasting}
              onClick={() => void act("done")}
            >
              {t("delivery.done")}
            </button>
          </div>
          <p className="mt-2 text-xs">{t("delivery.retryHint")}</p>
        </>
      )}
      {error && <p role="alert">{error}</p>}
    </section>
  );
}
