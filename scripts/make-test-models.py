# Writes three OBJ test models into the directory given: knot-no-uvs.obj (a torus knot with no
# UVs, for Auto-unwrap), crate-loose-uvs.obj (a box whose faces are bunched into one corner
# of the UV square, for Repack) and vase-no-uvs.obj (a lathed vase with a flat base and no UVs,
# for the projections). Used by scripts/captures/unwrap.sh, repack.sh and projections.sh.
#   python3 scripts/make-test-models.py target/models
import math, sys
out = sys.argv[1]
import os; os.makedirs(out, exist_ok=True)
# Torus knot, no UVs.
P, Q, N, M, R, r = 2, 3, 240, 24, 1.0, 0.28
def knot(t):
    rr = 0.5 * (2 + math.cos(Q * t))
    return (rr * math.cos(P * t), rr * math.sin(P * t), -0.5 * math.sin(Q * t))
def sub(a,b): return tuple(x-y for x,y in zip(a,b))
def cross(a,b): return (a[1]*b[2]-a[2]*b[1], a[2]*b[0]-a[0]*b[2], a[0]*b[1]-a[1]*b[0])
def norm(a):
    l = math.sqrt(sum(x*x for x in a)); return tuple(x/l for x in a)
with open(f"{out}/knot-no-uvs.obj", "w") as f:
    for i in range(N):
        t = 2*math.pi*i/N
        c = knot(t); d = norm(sub(knot(t+1e-3), c))
        n = norm(cross(d, (0,0,1))); b = cross(d, n)
        for j in range(M):
            a = 2*math.pi*j/M
            p = tuple(c[k] + r*(math.cos(a)*n[k] + math.sin(a)*b[k]) for k in range(3))
            f.write("v %f %f %f\n" % p)
    for i in range(N):
        for j in range(M):
            a = i*M + j + 1; b2 = ((i+1)%N)*M + j + 1
            c2 = ((i+1)%N)*M + (j+1)%M + 1; d2 = i*M + (j+1)%M + 1
            f.write(f"f {a} {b2} {c2}\nf {a} {c2} {d2}\n")
# Crate: a box whose six faces are packed wastefully into the bottom-left corner.
with open(f"{out}/crate-loose-uvs.obj", "w") as f:
    faces = [((1,0,0),(0,0,-1),(0,1,0)),((-1,0,0),(0,0,1),(0,1,0)),((0,1,0),(1,0,0),(0,0,-1)),
             ((0,-1,0),(1,0,0),(0,0,1)),((0,0,1),(1,0,0),(0,1,0)),((0,0,-1),(-1,0,0),(0,1,0))]
    vi = 1
    for k,(n,u,v) in enumerate(faces):
        cell = ((k%3)*0.14 + 0.02, (k//3)*0.14 + 0.02)
        for su,sv in [(-1,-1),(1,-1),(1,1),(-1,1)]:
            p = tuple(n[i] + u[i]*su + v[i]*sv for i in range(3))
            f.write("v %f %f %f\n" % p)
            f.write("vt %f %f\n" % (cell[0] + (su+1)*0.06, cell[1] + (sv+1)*0.06))
        f.write(f"f {vi}/{vi} {vi+1}/{vi+1} {vi+2}/{vi+2}\nf {vi}/{vi} {vi+2}/{vi+2} {vi+3}/{vi+3}\n")
        vi += 4
# Vase: a lathed profile, closed with a flat base and an open lip, no UVs.
with open(f"{out}/vase-no-uvs.obj", "w") as f:
    S, H = 48, 40
    for i in range(H + 1):
        y = -1.0 + 2.0 * i / H
        radius = 0.55 + 0.3 * math.sin((y + 1.0) * 1.6) - 0.25 * max(0.0, y - 0.4) * 2.0
        for j in range(S):
            a = 2 * math.pi * j / S
            f.write("v %f %f %f\n" % (radius * math.cos(a), y, radius * math.sin(a)))
    f.write("v 0 -1 0\n")
    centre = (H + 1) * S + 1
    for i in range(H):
        for j in range(S):
            a = i * S + j + 1; b = i * S + (j + 1) % S + 1
            c = (i + 1) * S + (j + 1) % S + 1; d = (i + 1) * S + j + 1
            f.write(f"f {a} {d} {c}\nf {a} {c} {b}\n")
    for j in range(S):
        f.write(f"f {centre} {j + 1} {(j + 1) % S + 1}\n")
