import { forwardRef, type ButtonHTMLAttributes } from "react";
import { cn } from "../../lib/utils";

type Variant = "default" | "ghost" | "outline";

const variants: Record<Variant, string> = {
  default: "bg-accent text-white hover:brightness-110",
  ghost: "text-fg hover:bg-hover",
  outline: "border border-line text-fg hover:bg-hover",
};

export interface ButtonProps extends ButtonHTMLAttributes<HTMLButtonElement> {
  variant?: Variant;
  icon?: boolean;
}

export const Button = forwardRef<HTMLButtonElement, ButtonProps>(
  ({ className, variant = "ghost", icon = false, type = "button", ...props }, ref) => (
    <button
      ref={ref}
      type={type}
      className={cn(
        "inline-flex h-6 shrink-0 items-center justify-center gap-1 rounded text-[12px] whitespace-nowrap transition-colors disabled:pointer-events-none disabled:opacity-40",
        icon ? "w-6" : "px-2",
        variants[variant],
        className,
      )}
      {...props}
    />
  ),
);
Button.displayName = "Button";
