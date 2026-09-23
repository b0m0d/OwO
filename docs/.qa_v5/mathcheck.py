# -*- coding: utf-8 -*-
import re
# arithmetic self-check of the published table
P, T, D = 240.0, 40000.0, 180000.0
IND  = {"保守": [300, 700, 1300], "基准": [400, 1200, 2800], "进取": [600, 2000, 5000]}
TEAM = {"保守": [0, 3, 8], "基准": [0, 5, 24], "进取": [0, 10, 40]}
DEP  = {"保守": [0, 0, 0], "基准": [0, 0, 2], "进取": [0, 1, 4]}
COST = {"保守": [40, 55, 80], "基准": [50, 105, 190], "进取": [65, 190, 260]}
PUB_REV = {"保守": [7.2, 28.8, 63.2], "基准": [9.6, 48.8, 199.2], "进取": [14.4, 106.0, 352.0]}
PUB_RES = {"保守": [-32.8, -26.2, -16.8], "基准": [-40.4, -56.2, 9.2], "进取": [-50.6, -84.0, 92.0]}
ok = True
for k in IND:
    for y in range(3):
        rev = round((IND[k][y]*P + TEAM[k][y]*T + DEP[k][y]*D)/1e4, 1)
        res = round(rev - COST[k][y], 1)
        if abs(rev - PUB_REV[k][y]) > 0.05:
            print("REV MISMATCH", k, y, rev, PUB_REV[k][y]); ok = False
        if abs(res - PUB_RES[k][y]) > 0.05:
            print("RES MISMATCH", k, y, res, PUB_RES[k][y]); ok = False
print("arithmetic consistent:", ok)
for k in IND:
    print(k, "rev", PUB_REV[k], "cost", COST[k], "res", PUB_RES[k], "cum2", round(PUB_RES[k][0]+PUB_RES[k][1],1))