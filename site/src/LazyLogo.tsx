import { useEffect, useState, type ComponentType } from "react";

// three.js is client-only and heavy: load it after first paint, not in the prerendered HTML.
export default function LazyLogo() {
  const [Logo, setLogo] = useState<ComponentType | null>(null);
  useEffect(() => {
    import("./JadeLogo").then((m) => setLogo(() => m.default));
  }, []);
  return Logo ? <Logo /> : <div className="size-full" role="img" aria-label="Xade jade logo" />;
}
