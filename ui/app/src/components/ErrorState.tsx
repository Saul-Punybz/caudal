interface Props {
  message: string;
}

/** Shown when the API cannot be reached at all (no prior data to fall back on). */
export function ErrorState({ message }: Props) {
  return (
    <div
      role="alert"
      className="flex flex-grow flex-col items-center justify-center gap-4 rounded-lg bg-surface-container-low p-10 text-center"
    >
      <span className="ms text-5xl text-error" aria-hidden="true">
        error
      </span>
      <h2 className="m-0 text-xl font-medium">Can't reach the Caudal API</h2>
      <p className="num m-0 max-w-md text-sm text-on-surface-variant">{message}</p>
      <p className="m-0 text-sm text-on-surface-variant">Retrying every second…</p>
    </div>
  );
}

/** A slim banner for when we have stale data but the latest poll failed. */
export function ErrorBanner({ message }: Props) {
  return (
    <div
      role="status"
      className="flex items-center gap-2 rounded-md bg-error-container px-4 py-2 text-sm text-on-error-container"
    >
      <span className="ms text-lg" aria-hidden="true">
        warning
      </span>
      Showing last known data — {message}
    </div>
  );
}
