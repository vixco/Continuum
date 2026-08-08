import type { Metadata } from "next";
import "./globals.css";
import "./hermes.css";

export const metadata: Metadata = {
  title: "Continuum",
  description: "Local context and permission layer for coding agents",
};

// Runs synchronously during HTML parse, before the body paints, so the
// correct palette is applied on the very first frame — no dark→light flash.
// Defaults to dark; flips to light only if that's the stored preference.
const themeBootScript = `(function(){try{var t=localStorage.getItem('continuum.theme');if(t==='light'){document.documentElement.setAttribute('data-theme','light');}else{document.documentElement.setAttribute('data-theme','dark');}}catch(e){document.documentElement.setAttribute('data-theme','dark');}})();`;

export default function RootLayout({ children }: { children: React.ReactNode }) {
  return (
    <html lang="en" data-theme="dark">
      <body className="antialiased">
        <script dangerouslySetInnerHTML={{ __html: themeBootScript }} />
        {children}
      </body>
    </html>
  );
}
