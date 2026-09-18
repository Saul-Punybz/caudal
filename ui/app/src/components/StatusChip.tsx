export type ChipTone = 'primary' | 'tertiary' | 'error' | 'neutral';

interface Props {
  icon: string;
  label: string;
  tone: ChipTone;
  spin?: boolean;
  className?: string;
}

const TONE_CLASSES: Record<ChipTone, string> = {
  // Orange = live and action (DESIGN.md).
  primary: 'bg-primary-container text-on-primary-container',
  // Gargoyle Gas = warm information: something is in progress.
  tertiary: 'bg-tertiary-container text-on-tertiary-container',
  // Error role for faults only.
  error: 'bg-error-container text-on-error-container',
  neutral: 'border border-outline text-on-surface-variant',
};

/** State pill for channels/restreams. Never color alone: icon + word
 * always ship with the tone, same rule as `LiveBadge`. */
export function StatusChip({ icon, label, tone, spin = false, className = '' }: Props) {
  return (
    <span
      className={`inline-flex h-6 items-center gap-1.5 rounded-full px-2.5 text-xs font-bold tracking-wide ${TONE_CLASSES[tone]} ${className}`}
    >
      <span className={`ms ms-fill text-sm ${spin ? 'animate-spin' : ''}`} aria-hidden="true">
        {icon}
      </span>
      {label}
    </span>
  );
}
