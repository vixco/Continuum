"use client";

import { useCallback, useState } from "react";

import { Shell } from "@/components/layout/Shell";
import { StartupIntro } from "@/components/layout/StartupIntro";

export default function Page() {
  const [introVisible, setIntroVisible] = useState(true);
  const completeIntro = useCallback(() => setIntroVisible(false), []);

  return (
    <>
      <Shell />
      {introVisible && <StartupIntro onComplete={completeIntro} />}
    </>
  );
}
