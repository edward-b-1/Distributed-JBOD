function stack(n,w,h,gx,gy, hs,ps, blue,orange, box,   i,k,x,y,c,H,P,mx,W,Ht,x0,yb,o){
  split(hs,H," "); split(ps,P," ");
  mx=0; for(i=1;i<=n;i++) if(H[i]>mx) mx=H[i];
  W=n*w+(n-1)*gx; Ht=mx*h+(mx-1)*gy;
  x0=(box-W)/2; yb=(box+Ht)/2; o="";
  for(i=1;i<=n;i++){
    x=x0+(i-1)*(w+gx);
    for(k=0;k<H[i];k++){
      y=yb-(k+1)*h-k*gy;
      c=(k==P[i])?orange:blue;
      o=o sprintf("    <rect x=\"%g\" y=\"%g\" width=\"%g\" height=\"%g\" rx=\"%g\" fill=\"%s\"/>\n",x,y,w,h,h/2,c);
    }
  }
  return o;
}
