import type { ReactNode } from 'react';

interface Props {
  icon: string;
  title: string;
  children?: ReactNode;
}

/** Generic "nothing here yet" panel for a screen that's reachable but has
 * no data — either the feature isn't configured on the server, or it is
 * but nothing has happened yet. Same shell as `EmptyState`, without
 * Overview's RTMP-specific instructions. */
export function EmptyPanel({ icon, title, children }: Props) {
  return (
    <div className="flex flex-grow flex-col items-center justify-center gap-3 rounded-lg bg-surface-container-low p-10 text-center">
      <span className="ms text-5xl text-on-surface-variant" aria-hidden="true">
        {icon}
      </span>
      <h2 className="m-0 text-xl font-medium">{title}</h2>
      {children && <div className="max-w-md text-sm text-on-surface-variant">{children}</div>}
    </div>
  );
}
