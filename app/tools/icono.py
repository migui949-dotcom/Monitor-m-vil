#!/usr/bin/env python3
"""Genera el icono de la app (adaptativo de Android, 108x108 dp) y una vista previa SVG.
Uso: python3 tools/icono.py  → res/drawable/ic_fondo.xml, res/drawable/ic_frente.xml, icono.svg"""
import os
D = os.path.join(os.path.dirname(__file__), "..")

def rr(x, y, w, h, r):
    return (f"M{x+r},{y} h{w-2*r} a{r},{r} 0 0 1 {r},{r} v{h-2*r} a{r},{r} 0 0 1 -{r},{r} "
            f"h-{w-2*r} a{r},{r} 0 0 1 -{r},-{r} v-{h-2*r} a{r},{r} 0 0 1 {r},-{r} z")

# (path, color o gradiente, opacidad)
GRAD_BG = ("lin", 0, 0, 108, 108, "#2a2f4a", "#16161e")
GRAD_SCR = ("lin", 30, 40, 78, 68, "#7aa2f7", "#bb9af7")
FONDO = [(rr(0, 0, 108, 108, 0), GRAD_BG, 1)]
CURSOR = "M61,51 l0,13 l3.2,-3 l2.4,5 l2.2,-1 l-2.4,-5 l4.4,0 z"
FRENTE = [
    (rr(25, 36, 58, 36, 7), "#0f0f17", 1),                 # cuerpo del móvil
    (rr(25, 36, 58, 36, 7), None, 1),                      # (borde: se dibuja aparte)
    (rr(29, 40, 50, 28, 3), GRAD_SCR, 1),                  # pantalla con fondo degradado
    (rr(33, 44, 24, 15, 2), "#1a1b26", 0.92),              # ventana
    (rr(33, 44, 24, 3.5, 1.5), "#c0caf5", 0.9),            # su barra de título
    (rr(29, 63.5, 50, 4.5, 1.5), "#1a1b26", 0.55),         # barra de tareas
    (rr(35, 52, 10, 1.6, 0.8), "#565f89", 1),              # líneas de texto en la ventana
    (rr(35, 55, 15, 1.6, 0.8), "#565f89", 1),
    (CURSOR, "#ffffff", 1),                                # cursor
]
BORDE = (rr(25, 36, 58, 36, 7), "#7aa2f7", 2.2)

def svg():
    defs, body = [], []
    def fill(c, i):
        if isinstance(c, tuple):
            _, x1, y1, x2, y2, a, b = c
            defs.append(f'<linearGradient id="g{i}" gradientUnits="userSpaceOnUse" x1="{x1}" y1="{y1}" x2="{x2}" y2="{y2}">'
                        f'<stop offset="0" stop-color="{a}"/><stop offset="1" stop-color="{b}"/></linearGradient>')
            return f"url(#g{i})"
        return c
    for i, (p, c, o) in enumerate(FONDO + FRENTE):
        if c is None:
            p, c, w = BORDE
            body.append(f'<path d="{p}" fill="none" stroke="{c}" stroke-width="{w}"/>'); continue
        body.append(f'<path d="{p}" fill="{fill(c, i)}" fill-opacity="{o}"/>')
    return ('<svg xmlns="http://www.w3.org/2000/svg" width="432" height="432" viewBox="0 0 108 108">'
            f'<defs>{"".join(defs)}</defs>{"".join(body)}</svg>')

def vector(items):
    out = ['<vector xmlns:android="http://schemas.android.com/apk/res/android" xmlns:aapt="http://schemas.android.com/aapt"',
           '    android:width="108dp" android:height="108dp" android:viewportWidth="108" android:viewportHeight="108">']
    for p, c, o in items:
        if c is None:
            p, c, w = BORDE
            out.append(f'  <path android:pathData="{p}" android:strokeColor="{c}" android:strokeWidth="{w}"/>'); continue
        if isinstance(c, tuple):
            _, x1, y1, x2, y2, a, b = c
            out.append(f'  <path android:pathData="{p}" android:fillAlpha="{o}">')
            out.append('    <aapt:attr name="android:fillColor">')
            out.append(f'      <gradient android:type="linear" android:startX="{x1}" android:startY="{y1}" android:endX="{x2}" android:endY="{y2}"'
                       f' android:startColor="{a}" android:endColor="{b}"/>')
            out.append('    </aapt:attr>\n  </path>')
        else:
            out.append(f'  <path android:pathData="{p}" android:fillColor="{c}" android:fillAlpha="{o}"/>')
    out.append('</vector>\n')
    return "\n".join(out)

open(os.path.join(D, "res/drawable/ic_fondo.xml"), "w").write(vector(FONDO))
open(os.path.join(D, "res/drawable/ic_frente.xml"), "w").write(vector(FRENTE))
open(os.path.join(D, "icono.svg"), "w").write(svg())
open(os.path.join(D, "res/mipmap-anydpi-v26/ic_launcher.xml"), "w").write(
    '<adaptive-icon xmlns:android="http://schemas.android.com/apk/res/android">\n'
    '  <background android:drawable="@drawable/ic_fondo"/>\n  <foreground android:drawable="@drawable/ic_frente"/>\n</adaptive-icon>\n')
