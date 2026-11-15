// Band colours: shared by graph handles and band chips.
export const BAND_COLORS = ["#e8a33d", "#2bb39a", "#e0598b", "#9b7bea", "#4fb3e8", "#7bc86c", "#ef7a5a", "#c9a227", "#8f9cf5", "#d077c9"];
export const bandColor = (i: number) => BAND_COLORS[i % BAND_COLORS.length];

export function hexA(hex: string, a: number): string {
  const h = hex.replace("#", "");
  const n = parseInt(h.length === 3 ? h.split("").map((c) => c + c).join("") : h, 16);
  return `rgba(${(n >> 16) & 255},${(n >> 8) & 255},${n & 255},${a})`;
}
