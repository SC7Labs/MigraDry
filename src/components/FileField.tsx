import { open } from "@tauri-apps/plugin-dialog";
import { useId, useState } from "react";

interface FileFieldProps {
  label: string;
  value: string;
  placeholder: string;
  /** Extensions offered by the native picker, e.g. `["db", "sqlite"]`. */
  extensions: string[];
  filterName: string;
  disabled: boolean;
  onChange: (value: string) => void;
}

/**
 * A path input with a native file picker beside it.
 *
 * The text field stays authoritative and editable: the picker is a convenience,
 * not the only way in, so a path can always be pasted or typed.
 */
export function FileField({
  label,
  value,
  placeholder,
  extensions,
  filterName,
  disabled,
  onChange,
}: FileFieldProps) {
  const inputId = useId();
  const [pickerError, setPickerError] = useState<string | null>(null);

  async function browse() {
    setPickerError(null);
    try {
      const selected = await open({
        multiple: false,
        directory: false,
        filters: [
          { name: filterName, extensions },
          { name: "All files", extensions: ["*"] },
        ],
      });
      if (typeof selected === "string") {
        onChange(selected);
      }
    } catch (error) {
      // A missing dialog backend must not take the whole form down; the path
      // can still be typed.
      setPickerError(
        error instanceof Error ? error.message : "The file picker is unavailable.",
      );
    }
  }

  return (
    <div className="field">
      <label className="field__label" htmlFor={inputId}>
        {label}
      </label>
      <div className="field__row">
        <input
          id={inputId}
          className="field__input"
          type="text"
          spellCheck={false}
          autoComplete="off"
          placeholder={placeholder}
          value={value}
          disabled={disabled}
          onChange={(event) => onChange(event.target.value)}
        />
        <button
          type="button"
          className="button button--ghost"
          onClick={browse}
          disabled={disabled}
        >
          Browse…
        </button>
      </div>
      {pickerError && <p className="field__error">{pickerError}</p>}
    </div>
  );
}
