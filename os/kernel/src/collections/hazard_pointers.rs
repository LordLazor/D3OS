/* ╔═════════════════════════════════════════════════════════════════════════╗
   ║ Module: hazard_pointers.rs                                              ║
   ╟─────────────────────────────────────────────────────────────────────────╢
   ║ The Hazard Pointers are based on Maged M. Michael's paper               ║
   ║ "Hazard Pointers: Safe Memory Reclamation for Lock-Free Objects"        ║
   ║ https://dl.acm.org/doi/10.1109/TPDS.2004.8                              ║
   ╟─────────────────────────────────────────────────────────────────────────╢
   ║ Author: Fabian Ruhland, 05.09.2025, HHU                                 ║
   ╚═════════════════════════════════════════════════════════════════════════╝
*/

// Based on Figure 4 of the paper "Hazard Pointers: Safe Memory Reclamation for Lock-Free Objects" by Maged M. Michael
