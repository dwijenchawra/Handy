import React from "react";
import { useTranslation } from "react-i18next";
import { ToggleSwitch } from "../ui/ToggleSwitch";
import { useSettings } from "../../hooks/useSettings";

interface UseAccessibilityContextProps {
  descriptionMode?: "inline" | "tooltip";
  grouped?: boolean;
}

export const UseAccessibilityContext: React.FC<UseAccessibilityContextProps> =
  React.memo(({ descriptionMode = "tooltip", grouped = false }) => {
    const { t } = useTranslation();
    const { getSetting, updateSetting, isUpdating } = useSettings();

    const enabled = getSetting("use_accessibility_context") || false;

    return (
      <ToggleSwitch
        checked={enabled}
        onChange={(enabled) =>
          updateSetting("use_accessibility_context", enabled)
        }
        isUpdating={isUpdating("use_accessibility_context")}
        label={t("settings.postProcessing.accessibilityContext.label")}
        description={t(
          "settings.postProcessing.accessibilityContext.description",
        )}
        descriptionMode={descriptionMode}
        grouped={grouped}
      />
    );
  });
