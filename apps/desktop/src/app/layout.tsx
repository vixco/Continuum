import type { Metadata } from "next";
import "./globals.css";

export const metadata: Metadata = {
  title: "Kairo",
  description: "The AI that knows when to act",
};

export default function RootLayout({
  children,
}: {
  children: React.ReactNode;
}) {
  return (
    <html lang="en">
      <body className="antialiased">{children}</body>
    </html>
  );
}
