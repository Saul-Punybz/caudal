interface Props {
  live: boolean;
  className?: string;
}

/** Never color-only: the word LIVE / Idle always ships with the pill. */
export function LiveBadge({ live, className = '' }: Props) {
  if (live) {
    return (
      <span
        aria-live="polite"
        className={`inline-flex items-center gap-1.5 h-6 rounded-full bg-primary-container px-2.5 text-xs font-bold tracking-wide text-on-primary-container ${className}`}
      >
        <span className="ms ms-fill text-sm" aria-hidden="true">
          radio_button_checked
        </span>
        LIVE
      </span>
    );
  }
  return (
    <span
      aria-live="polite"
      className={`inline-flex items-center gap-1.5 h-6 rounded-full border border-outline px-2.5 text-xs font-semibold text-on-surface-variant ${className}`}
    >
      <span className="ms text-sm" aria-hidden="true">
        pause_circle
      </span>
      Idle
    </span>
  );
}
