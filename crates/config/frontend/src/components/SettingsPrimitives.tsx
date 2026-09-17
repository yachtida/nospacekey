import {
  type ChangeEvent,
  type FocusEvent,
  type KeyboardEvent,
  type ReactNode,
  useEffect,
  useId,
  useRef,
  useState,
} from "react";
import type { FieldError } from "../bridge/types";

export function SettingsGroup({ title, children }: { title: string; children: ReactNode }) {
  return (
    <section className="settings-group" aria-labelledby={`group-${title}`}>
      <h2 id={`group-${title}`}>{title}</h2>
      <div className="settings-group-body">{children}</div>
    </section>
  );
}

export function SettingRow({
  id,
  title,
  description,
  effect,
  disabledReason,
  children,
}: {
  id: string;
  title: string;
  description?: ReactNode;
  effect?: string;
  disabledReason?: string;
  children: ReactNode;
}) {
  return (
    <div className="setting-row" id={`setting-${id}`} data-setting-id={id} tabIndex={-1}>
      <div className="setting-copy">
        <div className="setting-title">{title}</div>
        {description && <div className="setting-description">{description}</div>}
        {disabledReason && <div className="setting-disabled">{disabledReason}</div>}
        {effect && <div className="setting-effect">反映: {effect}</div>}
      </div>
      <div className="setting-control">{children}</div>
    </div>
  );
}

export function Switch({
  checked,
  onChange,
  label,
  disabled,
}: {
  checked: boolean;
  onChange: (checked: boolean) => void;
  label: string;
  disabled?: boolean;
}) {
  return (
    <label className="switch-label">
      <input
        className="switch-input"
        type="checkbox"
        checked={checked}
        onChange={(event) => onChange(event.target.checked)}
        disabled={disabled}
      />
      <span className="switch-track" aria-hidden="true"><span /></span>
      <span className="sr-only">{label}</span>
    </label>
  );
}

export function SegmentedChoice<T extends string>({
  label,
  value,
  options,
  onChange,
}: {
  label: string;
  value: T;
  options: Array<{ value: T; label: string }>;
  onChange: (value: T) => void;
}) {
  return (
    <fieldset className="segmented">
      <legend className="sr-only">{label}</legend>
      {options.map((option) => (
        <label key={option.value} className={value === option.value ? "selected" : ""}>
          <input
            type="radio"
            name={label}
            value={option.value}
            checked={value === option.value}
            onChange={() => onChange(option.value)}
          />
          <span>{option.label}</span>
        </label>
      ))}
    </fieldset>
  );
}

export function InlineError({ errors, field }: { errors: FieldError[]; field: string }) {
  const messages = errors.filter((error) => error.field === field);
  if (!messages.length) return null;
  return (
    <div className="inline-error" role="alert">
      {messages.map((error, index) => <div key={`${error.message}-${index}`}>{error.message}</div>)}
    </div>
  );
}

type CommitFieldProps = {
  value: string | number;
  onCommit: (value: string) => void;
  label: string;
  type?: "text" | "number";
  min?: number;
  max?: number;
  step?: number;
  placeholder?: string;
};

export function CommitField({
  value,
  onCommit,
  label,
  type = "text",
  min,
  max,
  step,
  placeholder,
}: CommitFieldProps) {
  const id = useId();
  const inputRef = useRef<HTMLInputElement>(null);
  const [draft, setDraft] = useState(String(value));
  const [composing, setComposing] = useState(false);
  const dirty = useRef(false);
  const awaitingConfirmation = useRef(false);

  const publishDirty = (value: boolean) => {
    inputRef.current?.setAttribute("data-commit-dirty", String(value));
    window.dispatchEvent(new Event("settings-draft-change"));
  };

  useEffect(() => {
    const confirmed = String(value);
    if (awaitingConfirmation.current) {
      if (confirmed === draft) awaitingConfirmation.current = false;
      return;
    }
    if (!dirty.current) setDraft(confirmed);
  }, [draft, value]);

  const commit = () => {
    if (composing || !dirty.current) return;
    if (type === "number" && (draft.trim() === "" || draft === "-" || !Number.isFinite(Number(draft)))) {
      return;
    }
    dirty.current = false;
    publishDirty(false);
    awaitingConfirmation.current = true;
    onCommit(draft);
  };
  const onChange = (event: ChangeEvent<HTMLInputElement>) => {
    dirty.current = true;
    publishDirty(true);
    setDraft(event.target.value);
  };
  const onBlur = (_event: FocusEvent<HTMLInputElement>) => commit();
  const onKeyDown = (event: KeyboardEvent<HTMLInputElement>) => {
    if (event.key === "Enter" && !event.nativeEvent.isComposing && !composing) {
      event.preventDefault();
      commit();
    }
  };
  return (
    <label className="commit-field" htmlFor={id}>
      <span className="sr-only">{label}</span>
      <input
        ref={inputRef}
        id={id}
        type={type}
        value={draft}
        min={min}
        max={max}
        step={step}
        placeholder={placeholder}
        data-commit-dirty="false"
        onChange={onChange}
        onBlur={onBlur}
        onKeyDown={onKeyDown}
        onCompositionStart={() => setComposing(true)}
        onCompositionEnd={() => setComposing(false)}
      />
    </label>
  );
}

export function StatusMessage({
  tone = "neutral",
  children,
}: {
  tone?: "neutral" | "success" | "warning" | "error";
  children: ReactNode;
}) {
  return <div className={`status-message ${tone}`} role={tone === "error" ? "alert" : "status"}>{children}</div>;
}

export function EditorDialog({
  open,
  title,
  dirty = false,
  onClose,
  children,
}: {
  open: boolean;
  title: string;
  dirty?: boolean;
  onClose: () => void;
  children: ReactNode;
}) {
  const ref = useRef<HTMLDialogElement>(null);
  const requestClose = () => {
    if (dirty && !window.confirm("保存していない変更を破棄しますか？")) return;
    onClose();
  };
  useEffect(() => {
    const dialog = ref.current;
    if (!dialog) return;
    if (open && !dialog.open) dialog.showModal();
    if (!open && dialog.open) dialog.close();
  }, [open]);
  useEffect(() => {
    const discard = () => { if (open) onClose(); };
    window.addEventListener("settings-discard-editors", discard);
    return () => window.removeEventListener("settings-discard-editors", discard);
  }, [onClose, open]);
  useEffect(() => {
    window.dispatchEvent(new Event("settings-editor-change"));
  }, [dirty, open]);
  return (
    <dialog ref={ref} className="editor-dialog" data-dirty={dirty} onCancel={(event) => { event.preventDefault(); requestClose(); }} onClose={onClose}>
      <div className="dialog-head">
        <h2>{title}</h2>
        <button type="button" className="quiet icon-close" onClick={requestClose} aria-label="閉じる">×</button>
      </div>
      {children}
    </dialog>
  );
}
