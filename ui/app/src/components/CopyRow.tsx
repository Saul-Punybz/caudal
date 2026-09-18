import { useState } from 'react';

interface Props {
  icon: string;
  label: string;
  tag?: string;
  url: string;
  disabled?: boolean;
}

/** One copyable URL row, used for both the "publish here" and "play it" lists. */
export function CopyRow({ icon, label, tag, url, disabled = false }: Props) {
  const [copied, setCopied] = useState(false);

  async function copy() {
    try {
      await navigator.clipboard.writeText(url);
      setCopied(true);
      setTimeout(() => setCopied(false), 1500);
    } catch {
      // Clipboard API can be unavailable (no permission, insecure context);
      // the URL is still shown and selectable by hand.
    }
  }

  return (
    <div
      className={`flex items-center gap-3 rounded-md bg-surface-container-high py-2.5 pl-4 pr-2 ${
        disabled ? 'opacity-50' : ''
      }`}
    >
      <span className="ms text-on-surface-variant" aria-hidden="true">
        {icon}
      </span>
      <div className="min-w-0 flex-grow">
        <div className="flex items-center gap-2 text-sm font-semibold">
          {label}
          {tag && (
            <span className="rounded-sm border border-outline-variant px-2 py-0.5 text-xs font-semibold text-on-surface-variant">
              {tag}
            </span>
          )}
        </div>
        <div className="num overflow-hidden text-ellipsis whitespace-nowrap text-xs text-on-surface-variant">
          {url}
        </div>
      </div>
      <button
        type="button"
        onClick={copy}
        disabled={disabled}
        aria-label={copied ? `Copied ${label} URL` : `Copy ${label} URL`}
        className="state-layer flex flex-shrink-0 items-center justify-center rounded-full border-0 bg-transparent text-on-surface-variant disabled:cursor-not-allowed"
        style={{ width: 44, height: 44 }}
      >
        <span className="ms text-xl" aria-hidden="true">
          {copied ? 'check' : 'content_copy'}
        </span>
      </button>
    </div>
  );
}
