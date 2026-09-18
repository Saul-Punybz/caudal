import { NavLink } from 'react-router-dom';

interface RailLinkProps {
  to: string;
  icon: string;
  label: string;
}

function RailLink({ to, icon, label }: RailLinkProps) {
  return (
    <NavLink
      to={to}
      end={to === '/'}
      className="state-layer flex flex-col items-center gap-1 rounded-lg py-1 text-xs font-medium text-on-surface-variant no-underline aria-[current=page]:text-on-surface"
      style={{ minHeight: 44, minWidth: 44 }}
    >
      {({ isActive }) => (
        <>
          <span
            className={`flex h-8 w-14 items-center justify-center rounded-full ${
              isActive ? 'bg-secondary-container text-on-secondary-container' : ''
            }`}
          >
            <span className={`ms ${isActive ? 'ms-fill' : ''}`} aria-hidden="true">
              {icon}
            </span>
          </span>
          {label}
        </>
      )}
    </NavLink>
  );
}

function RailPlaceholder({ icon, label }: { icon: string; label: string }) {
  return (
    <button
      type="button"
      disabled
      aria-label={`${label} (coming soon)`}
      title={`${label} — coming soon`}
      className="flex cursor-not-allowed flex-col items-center gap-1 rounded-lg border-0 bg-transparent py-1 text-xs font-medium text-on-surface-variant opacity-40"
      style={{ minHeight: 44, minWidth: 44 }}
    >
      <span className="flex h-8 w-14 items-center justify-center rounded-full">
        <span className="ms" aria-hidden="true">
          {icon}
        </span>
      </span>
      {label}
    </button>
  );
}

/** Left navigation rail. Only Overview, Publish and Streams have real
 * screens today; the rest of MistServer's parity list is shown disabled,
 * per DESIGN.md's screen inventory, rather than built as fake pages. */
export function NavRail() {
  return (
    <nav
      aria-label="Main"
      className="flex w-[88px] flex-shrink-0 flex-col items-center gap-3 bg-surface py-5"
    >
      <div
        aria-hidden="true"
        className="mb-5 flex h-12 w-12 items-center justify-center rounded-2xl bg-primary-container text-on-primary-container"
      >
        <svg width="28" height="28" viewBox="0 0 28 28" fill="none">
          <path
            d="M4 10c4-4 8-4 10 0s6 4 10 0M4 18c4-4 8-4 10 0s6 4 10 0"
            stroke="currentColor"
            strokeWidth="3"
            strokeLinecap="round"
          />
        </svg>
      </div>
      <RailLink to="/" icon="space_dashboard" label="Overview" />
      <RailLink to="/publish" icon="videocam" label="Publish" />
      <RailPlaceholder icon="input" label="Ingest" />
      <RailPlaceholder icon="cast" label="Push" />
      <RailPlaceholder icon="bolt" label="Triggers" />
      <RailPlaceholder icon="monitoring" label="Stats" />
      <RailPlaceholder icon="terminal" label="Logs" />
      <RailPlaceholder icon="key" label="Keys" />
      <div className="flex-grow" />
      <RailPlaceholder icon="settings" label="Settings" />
    </nav>
  );
}
