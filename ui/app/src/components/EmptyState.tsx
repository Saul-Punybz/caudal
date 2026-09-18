const RTMP_URL = `rtmp://${typeof window !== 'undefined' ? window.location.hostname : 'localhost'}:1935/live/demo`;
const FFMPEG_CMD = `ffmpeg -re -f lavfi -i testsrc2=size=1280x720:rate=30 -f lavfi -i sine=sample_rate=48000 \\\n  -c:v libx264 -g 60 -c:a aac -f flv ${RTMP_URL}`;

/** Shown when the API is reachable but reports zero streams. */
export function EmptyState() {
  return (
    <div className="flex flex-grow flex-col items-center justify-center gap-4 rounded-lg bg-surface-container-low p-10 text-center">
      <span className="ms text-5xl text-on-surface-variant" aria-hidden="true">
        sensors_off
      </span>
      <h2 className="m-0 text-xl font-medium">No streams yet</h2>
      <p className="m-0 max-w-md text-sm text-on-surface-variant">
        Publish to the RTMP endpoint below and it will show up here within a second.
      </p>
      <div className="flex w-full max-w-xl flex-col gap-2 rounded-md bg-surface-container-high p-4 text-left">
        <div className="text-xs font-semibold uppercase tracking-wide text-on-surface-variant">
          RTMP URL
        </div>
        <code className="num overflow-x-auto whitespace-pre text-sm">{RTMP_URL}</code>
        <div className="mt-2 text-xs font-semibold uppercase tracking-wide text-on-surface-variant">
          ffmpeg test source
        </div>
        <pre className="num m-0 overflow-x-auto whitespace-pre-wrap break-all text-sm">
          {FFMPEG_CMD}
        </pre>
      </div>
    </div>
  );
}
