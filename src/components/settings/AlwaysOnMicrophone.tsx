import React from "react";
import { useTranslation } from "react-i18next";
import { ToggleSwitch } from "../ui/ToggleSwitch";

interface AlwaysOnMicrophoneProps {
  descriptionMode?: "inline" | "tooltip";
  grouped?: boolean;
}

export const AlwaysOnMicrophone: React.FC<AlwaysOnMicrophoneProps> = React.memo(
  ({ descriptionMode = "tooltip", grouped = false }) => {
    const { t } = useTranslation();

    return (
      <ToggleSwitch
        checked={true}
        disabled={true}
        onChange={() => {}}
        label={t("settings.debug.alwaysOnMicrophone.label")}
        description={t(
          "settings.debug.alwaysOnMicrophone.captureReadyDescription",
        )}
        descriptionMode={descriptionMode}
        grouped={grouped}
      />
    );
  },
);
