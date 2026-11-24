import { useEffect, useRef } from "react";
import * as THREE from "three";
import { RoomEnvironment } from "three/examples/jsm/environments/RoomEnvironment.js";

const COUNT = 10;
const PALETTE = ["#16a34a", "#047857", "#34d399", "#059669", "#10b981", "#065f46", "#15803d"];
const BG = "#020617";

// Raw, irregular gem shape: a low-poly solid with every vertex nudged (shared vertices move together).
function rawGem(seed: number) {
  const bases = [
    () => new THREE.IcosahedronGeometry(1, 0),
    () => new THREE.OctahedronGeometry(1, 0),
    () => new THREE.DodecahedronGeometry(1, 0),
    () => new THREE.TetrahedronGeometry(1.2, 0),
  ];
  const geo = bases[seed % bases.length]();
  const pos = geo.attributes.position;
  const moved = new Map<string, THREE.Vector3>();
  for (let i = 0; i < pos.count; i++) {
    const key = `${pos.getX(i).toFixed(3)},${pos.getY(i).toFixed(3)},${pos.getZ(i).toFixed(3)}`;
    if (!moved.has(key)) {
      const j = 0.28;
      moved.set(key, new THREE.Vector3(
        pos.getX(i) * (1 + (Math.random() - 0.5) * j * 2),
        pos.getY(i) * (1 + (Math.random() - 0.5) * j * 2),
        pos.getZ(i) * (1 + (Math.random() - 0.5) * j * 2),
      ));
    }
    const v = moved.get(key)!;
    pos.setXYZ(i, v.x, v.y, v.z);
  }
  geo.computeVertexNormals();
  return geo;
}

// A few raw jade gems drifting down behind the page, with mirror-like glints as they tumble.
export default function JadeRain() {
  const host = useRef<HTMLDivElement>(null);

  useEffect(() => {
    const el = host.current!;
    const renderer = new THREE.WebGLRenderer({ antialias: true, alpha: true, powerPreference: "low-power" });
    renderer.setPixelRatio(Math.min(devicePixelRatio, 1.5));
    el.appendChild(renderer.domElement);

    const scene = new THREE.Scene();
    scene.fog = new THREE.Fog(BG, 24, 52);
    const pmrem = new THREE.PMREMGenerator(renderer);
    scene.environment = pmrem.fromScene(new RoomEnvironment(), 0.04).texture;

    // hard, bright lights: a flat facet only catches them for an instant, like a mirror flash
    const keyLight = new THREE.DirectionalLight("#ffffff", 9);
    keyLight.position.set(-6, 9, 12);
    const rimLight = new THREE.DirectionalLight("#d1fae5", 6);
    rimLight.position.set(8, -4, 8);
    scene.add(keyLight, rimLight);

    const camera = new THREE.PerspectiveCamera(40, 1, 0.1, 100);
    camera.position.z = 30;

    const mats = PALETTE.map(
      (c) => new THREE.MeshPhysicalMaterial({
        color: c, flatShading: true, roughness: 0.03, metalness: 0.3,
        clearcoat: 1, clearcoatRoughness: 0.03, specularIntensity: 1, envMapIntensity: 0.9,
      }),
    );

    const bounds = { x: 20, y: 14 };
    const items = Array.from({ length: COUNT }, (_, i) => {
      const mesh = new THREE.Mesh(rawGem(i), mats[i % mats.length]);
      const s = 0.18 + Math.random() * 0.3;
      mesh.scale.set(s * (0.7 + Math.random() * 0.8), s * (0.7 + Math.random() * 1.2), s * (0.7 + Math.random() * 0.8));
      scene.add(mesh);
      return {
        mesh,
        x: (Math.random() * 2 - 1) * bounds.x,
        y: (Math.random() * 2 - 1) * bounds.y,
        z: -26 + Math.random() * 27,
        speed: 0.45 + Math.random() * 0.9,
        spin: new THREE.Vector3(Math.random() - 0.5, Math.random() - 0.5, Math.random() - 0.5).multiplyScalar(1.4),
      };
    });

    const draw = (dt: number) => {
      for (const it of items) {
        it.y -= it.speed * dt;
        if (it.y < -bounds.y) {
          it.y = bounds.y;
          it.x = (Math.random() * 2 - 1) * bounds.x;
        }
        it.mesh.rotation.x += it.spin.x * dt;
        it.mesh.rotation.y += it.spin.y * dt;
        it.mesh.rotation.z += it.spin.z * dt;
        it.mesh.position.set(it.x, it.y, it.z);
      }
      camera.position.y = -scrollY * 0.006;
      renderer.render(scene, camera);
    };

    const resize = () => {
      renderer.setSize(innerWidth, innerHeight);
      camera.aspect = innerWidth / innerHeight;
      camera.updateProjectionMatrix();
    };
    resize();
    addEventListener("resize", resize);

    const reduce = matchMedia("(prefers-reduced-motion: reduce)").matches;
    let raf = 0;
    let last = performance.now();
    const tick = (now: number) => {
      draw(Math.min((now - last) / 1000, 0.05));
      last = now;
      raf = requestAnimationFrame(tick);
    };
    const onScroll = () => draw(0);
    if (reduce) {
      draw(0);
      addEventListener("scroll", onScroll, { passive: true });
    } else {
      raf = requestAnimationFrame(tick);
    }

    return () => {
      cancelAnimationFrame(raf);
      removeEventListener("resize", resize);
      removeEventListener("scroll", onScroll);
      items.forEach((it) => it.mesh.geometry.dispose());
      mats.forEach((m) => m.dispose());
      pmrem.dispose();
      renderer.dispose();
      renderer.domElement.remove();
    };
  }, []);

  return <div ref={host} aria-hidden="true" className="pointer-events-none fixed inset-0 opacity-70" />;
}
