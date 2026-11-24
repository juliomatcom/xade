import { useEffect, useRef } from "react";
import * as THREE from "three";
import { RoomEnvironment } from "three/examples/jsm/environments/RoomEnvironment.js";
import { RoundedBoxGeometry } from "three/examples/jsm/geometries/RoundedBoxGeometry.js";

// Tile layout from assets/xade-icon.svg (512 grid): x, y, size, gradient [from, mid, to].
const J1 = ["#16a34a", "#047857", "#022c22"];
const J2 = ["#34d399", "#059669", "#064e3b"];
const J3 = ["#10b981", "#065f46", "#022c22"];
const J4 = ["#059669", "#15803d", "#064e3b"];
const TILES: [number, number, number, string[]][] = [
  [0, 0, 248, J1], [264, 0, 116, J2], [396, 0, 116, J3], [264, 132, 116, J4],
  [396, 132, 116, J1], [0, 264, 116, J3], [132, 264, 116, J4], [264, 264, 248, J2],
  [0, 396, 116, J1], [132, 396, 116, J2],
];

function gradientColor(stops: string[], t: number) {
  const [a, b, c] = stops.map((s) => new THREE.Color(s));
  const k = Math.min(Math.max(t, 0), 1);
  return k < 0.5 ? a.clone().lerp(b, k * 2) : b.clone().lerp(c, (k - 0.5) * 2);
}

function tileGeometry(size: number, depth: number, stops: string[]) {
  const geo = new RoundedBoxGeometry(size, size, depth, 3, Math.min(size * 0.06, 8));
  const pos = geo.attributes.position;
  const colors = new Float32Array(pos.count * 3);
  for (let i = 0; i < pos.count; i++) {
    // diagonal gradient, top-left (light) to bottom-right (dark)
    const t = (pos.getX(i) / size - pos.getY(i) / size) / 2 + 0.5;
    gradientColor(stops, t).toArray(colors, i * 3);
  }
  geo.setAttribute("color", new THREE.BufferAttribute(colors, 3));
  return geo;
}

export default function JadeLogo() {
  const host = useRef<HTMLDivElement>(null);

  useEffect(() => {
    const el = host.current!;
    const renderer = new THREE.WebGLRenderer({ antialias: true, alpha: true });
    renderer.setPixelRatio(Math.min(devicePixelRatio, 2));
    renderer.toneMapping = THREE.NoToneMapping;
    renderer.toneMappingExposure = 1;
    el.appendChild(renderer.domElement);

    const scene = new THREE.Scene();
    const pmrem = new THREE.PMREMGenerator(renderer);
    scene.environment = pmrem.fromScene(new RoomEnvironment(), 0.04).texture;

    const camera = new THREE.PerspectiveCamera(30, 1, 10, 3000);
    camera.position.set(0, 0, 1250);

    const key = new THREE.DirectionalLight("#ffffff", 0.5);
    key.position.set(-400, 500, 700);
    scene.add(key);

    const logo = new THREE.Group();
    scene.add(logo);

    // slab: dark metal chassis
    const slab = new THREE.Mesh(
      new RoundedBoxGeometry(512, 512, 40, 6, 90),
      new THREE.MeshPhysicalMaterial({ color: "#0b1220", metalness: 0.85, roughness: 0.35, clearcoat: 0.6 }),
    );
    logo.add(slab);

    const jade = new THREE.MeshPhysicalMaterial({
      vertexColors: true, roughness: 0.45, metalness: 0.0,
      clearcoat: 0.5, clearcoatRoughness: 0.2, envMapIntensity: 0.25,
    });
    const geos: THREE.BufferGeometry[] = [slab.geometry];
    for (const [x, y, size, stops] of TILES) {
      const g = tileGeometry(size, 36, stops);
      geos.push(g);
      const m = new THREE.Mesh(g, jade);
      // SVG origin is top-left, y down
      m.position.set(x + size / 2 - 256, 256 - (y + size / 2), 28);
      logo.add(m);
    }

    const target = { x: 0, y: 0 };
    const onMove = (e: PointerEvent) => {
      target.y = (e.clientX / innerWidth - 0.5) * 1.1;
      target.x = (e.clientY / innerHeight - 0.5) * 0.8;
    };
    addEventListener("pointermove", onMove);

    const resize = () => {
      const { clientWidth: w, clientHeight: h } = el;
      renderer.setSize(w, h);
      camera.aspect = w / h;
      camera.updateProjectionMatrix();
    };
    const ro = new ResizeObserver(resize);
    ro.observe(el);
    resize();

    const reduce = matchMedia("(prefers-reduced-motion: reduce)").matches;
    const t0 = performance.now();
    let raf = 0;
    const tick = () => {
      const t = (performance.now() - t0) / 1000;
      const idleY = reduce ? 0 : Math.sin(t * 0.6) * 0.18;
      const idleX = reduce ? 0 : Math.cos(t * 0.45) * 0.06;
      logo.rotation.y += (target.y + idleY - logo.rotation.y) * 0.06;
      logo.rotation.x += (target.x + idleX - logo.rotation.x) * 0.06;
      logo.position.y = reduce ? 0 : Math.sin(t * 0.9) * 8;
      renderer.render(scene, camera);
      raf = requestAnimationFrame(tick);
    };
    tick();

    return () => {
      cancelAnimationFrame(raf);
      removeEventListener("pointermove", onMove);
      ro.disconnect();
      geos.forEach((g) => g.dispose());
      jade.dispose();
      pmrem.dispose();
      renderer.dispose();
      renderer.domElement.remove();
    };
  }, []);

  return <div ref={host} className="size-full" aria-label="Xade jade logo" role="img" />;
}
