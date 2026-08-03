"use client";

// StreamingText — markdown rendering for in-flight assistant text.
//
// During streaming, the model is producing tokens faster than React can
// re-render at 60fps without thrashing. The trick we use is: only re-parse
// markdown every N characters of new text, not every token. The text itself
// is always up-to-date (plain `<span>` reveal), but the structured view
// (lists, code fences, links) only re-flows when enough new text has
// arrived to make the parser's output materially different. Once `isStreaming`
// flips off, we always render the final markdown immediately.

import { memo, useEffect, useMemo, useRef, useState } from "react";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import { clsx } from "clsx";

interface StreamingTextProps {
  text: string;
  isStreaming?: boolean;
  className?: string;
}

// How many new characters between full markdown re-renders. Big enough to
// absorb a token burst; small enough that code fences close within a beat.
const REPARSE_INTERVAL = 64;

const MARKDOWN_CLASSES =
  "break-words text-sm leading-relaxed " +
  "[&_p:not(:last-child)]:mb-2 [&_p]:m-0 " +
  "[&_ul]:my-1.5 [&_ul]:list-disc [&_ul]:pl-5 [&_ol]:my-1.5 [&_ol]:list-decimal [&_ol]:pl-5 [&_li]:mb-0.5 " +
  "[&_a]:text-amber-300 [&_a]:underline [&_a]:underline-offset-2 " +
  "[&_strong]:font-semibold [&_strong]:text-ink " +
  "[&_em]:italic " +
  "[&_h1]:mb-2 [&_h1]:text-base [&_h1]:font-semibold " +
  "[&_h2]:mb-1.5 [&_h2]:text-sm [&_h2]:font-semibold " +
  "[&_h3]:mb-1 [&_h3]:text-sm [&_h3]:font-semibold " +
  "[&_blockquote]:border-l-2 [&_blockquote]:border-amber-500/30 [&_blockquote]:pl-3 [&_blockquote]:italic [&_blockquote]:text-ink-muted " +
  "[&_table]:my-2 [&_table]:w-full [&_table]:border-collapse [&_th]:border [&_th]:border-bg-border [&_th]:bg-bg-elevated [&_th]:px-2 [&_th]:py-1 [&_th]:text-left [&_td]:border [&_td]:border-bg-border [&_td]:px-2 [&_td]:py-1 " +
  "[&_pre]:my-2 [&_pre]:overflow-x-auto [&_pre]:rounded-md [&_pre]:border [&_pre]:border-bg-border [&_pre]:bg-black/40 [&_pre]:px-3 [&_pre]:py-2.5 " +
  "[&_code]:font-mono [&_code]:text-[12.5px] " +
  "[&_:not(pre)>code]:rounded [&_:not(pre)>code]:bg-bg-elevated [&_:not(pre)>code]:px-1 [&_:not(pre)>code]:py-0.5 [&_:not(pre)>code]:text-amber-200/90 " +
  "[&_hr]:my-3 [&_hr]:border-bg-border";

function StreamingTextImpl({ text, isStreaming, className }: StreamingTextProps) {
  // We always store the latest text but only re-render the parsed markdown
  // every REPARSE_INTERVAL characters while streaming. The raw text itself
  // is shown in a hidden layer for accessibility (and used when not yet
  // re-parsed).
  const [parseVersion, setParseVersion] = useState(0);
  const [lastParsedLen, setLastParsedLen] = useState(0);
  const textRef = useRef(text);

  useEffect(() => {
    textRef.current = text;
    if (!isStreaming) {
      // Force a final parse on stream end.
      setParseVersion((v) => v + 1);
      setLastParsedLen(text.length);
      return;
    }
    if (text.length - lastParsedLen >= REPARSE_INTERVAL) {
      setParseVersion((v) => v + 1);
      setLastParsedLen(text.length);
    }
  }, [text, isStreaming, lastParsedLen]);

  // The rendered markdown is derived from text on each reparse; on the
  // interim frames we just show the raw text in a wrapper.
  const rendered = useMemo(
    () => (
      <ReactMarkdown remarkPlugins={[remarkGfm]} components={markdownComponents}>
        {text}
      </ReactMarkdown>
    ),
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [parseVersion]
  );

  return (
    <div className={clsx(MARKDOWN_CLASSES, className)}>
      {rendered}
      {isStreaming && (
        <span
          className="ml-0.5 inline-block h-3 w-[2px] -mb-0.5 animate-pulse rounded-sm bg-amber-400/80 align-middle"
          aria-hidden
        />
      )}
    </div>
  );
}

export const StreamingText = memo(StreamingTextImpl);

// Custom component overrides — keep them inline so we don't re-import the
// react-markdown internals anywhere else and so the spacing/typography is
// consistent with the rest of the dashboard.
const markdownComponents = {
  a({ href, children, ...props }: { href?: string; children?: React.ReactNode }) {
    return (
      <a href={href} target="_blank" rel="noreferrer" {...props}>
        {children}
      </a>
    );
  },
};
