import { useEffect, useState, type ComponentType } from "react";

// Decorative and client-only: load after first paint so it never delays content.
export default function LazyRain() {
  const [Rain, setRain] = useState<ComponentType | null>(null);
  useEffect(() => {
    import("./JadeRain").then((m) => setRain(() => m.default));
  }, []);
  return Rain ? <Rain /> : null;
}
