import { useEffect, useState } from "react";

const QUERY = "(max-width: 720px)";

/** Tracks the same breakpoint the CSS uses for the mobile layout, so JS-level
 *  decisions (full-screen editor, swipe-to-delete) stay in lockstep with it. */
export function useIsMobile(): boolean {
  const [isMobile, setIsMobile] = useState(
    () => typeof window !== "undefined" && window.matchMedia(QUERY).matches,
  );
  useEffect(() => {
    const mq = window.matchMedia(QUERY);
    const onChange = () => setIsMobile(mq.matches);
    mq.addEventListener("change", onChange);
    return () => mq.removeEventListener("change", onChange);
  }, []);
  return isMobile;
}
