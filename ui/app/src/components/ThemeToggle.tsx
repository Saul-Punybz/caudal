import { useTheme } from '../theme';

export function ThemeToggle() {
  const { effective, toggle } = useTheme();
  return (
    <button
      type="button"
      onClick={toggle}
      aria-label={effective === 'dark' ? 'Switch to light theme' : 'Switch to dark theme'}
      className="state-layer flex items-center justify-center rounded-full border-0 bg-transparent text-on-surface-variant"
      style={{ width: 48, height: 48 }}
    >
      <span className="ms" aria-hidden="true">
        {effective === 'dark' ? 'light_mode' : 'dark_mode'}
      </span>
    </button>
  );
}
